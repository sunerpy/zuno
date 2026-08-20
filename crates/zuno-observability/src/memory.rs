//! Resident-memory history and the four-level attribution of a memory incident.
//!
//! # What an incident is here
//!
//! A **runtime alert**, not a gate. G1 through G4 in
//! `.omo/plans/memory-perf-optimization.md` decide whether a build regressed; an
//! incident says a *running* process has reached a size or a growth rate that a
//! human should look at. Confusing the two would either fail builds on healthy
//! sessions or let a leaking process run silently, so the thresholds here are
//! derived independently of the gate ceilings and are never compared against them.
//!
//! # Why the reference implementation's thresholds could not be adopted
//!
//! It warns at 1 GiB of PSS and escalates at 2 GiB. This project's own M1
//! measurement puts a *healthy* 931-message W-real session at a 1,198,872 KiB
//! (1.143 GiB) median peak, with the highest of its five runs at 1,549,164 KiB
//! (1.477 GiB) — so a 1 GiB warning would fire on every normal large session and
//! a 2 GiB critical would fire on the ordinary run-to-run spread. §10.1 forbids
//! adopting the reference's figures for exactly this reason; the thresholds below
//! are multiples of this project's measured peaks.
//!
//! # Why attribution comes from `/proc` and not from the allocator
//!
//! The reference reads jemalloc's `retained` statistic through `jemalloc-ctl`.
//! That crate is not in this workspace's dependency graph, and adding one to
//! support a diagnostic would be a poor trade. Linux publishes the split this
//! attribution actually needs — `RssAnon`, `RssFile` and `RssShmem` in
//! `/proc/self/status`, which sum to `VmRSS` — so the levels below are measured
//! rather than inferred. What is lost is the ability to separate "allocator is
//! holding freed pages" from "the heap is genuinely live"; the
//! `dirty_decay_ms:1000,muzzy_decay_ms:1000` tuning in `.cargo/config.toml` makes
//! that distinction short-lived anyway, since freed pages return within about a
//! second. [`Attribution::AnonymousHeap`] therefore names both, and says so.
//!
//! # Bounded by construction
//!
//! [`MemoryRing`] holds [`MEMORY_RING_SAMPLES`] fixed-size records and nothing
//! else. Its capacity is allocated once at construction, so a process that runs
//! for a week uses exactly the same bytes as one that ran for a minute.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How many samples the short-term ring retains.
///
/// Protects against: an unbounded diagnostic history in a process that runs for
/// days, which is the failure class this plan exists to remove rather than move.
///
/// Every [`MemorySample`] is fixed-size, so this count is the byte bound:
/// 512 records at 48 bytes is 24,576 bytes, 0.002% of M1's 1,198,872 KiB W-real
/// median. The count is chosen with [`SAMPLE_EVERY`] and [`GROWTH_WINDOW`]
/// together — see [`ring_covers_the_growth_window`].
pub const MEMORY_RING_SAMPLES: usize = 512;

/// How often the ring is expected to be fed.
///
/// Matches the 2-second interval `crates/zuno-testkit/src/perf/workload.rs` uses
/// for the frozen gates, so a runtime trace and a gate trace describe growth at
/// the same resolution and can be compared directly.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(2);

/// The span over which growth is judged.
///
/// Protects against: a burst that looks like a leak. A single turn can allocate
/// hundreds of MiB and release it; growth is only evidence when it survives a
/// window several minutes wide.
///
/// 15 minutes at [`SAMPLE_EVERY`] is 450 samples, which fits inside
/// [`MEMORY_RING_SAMPLES`] with 62 samples of margin. That relationship is the
/// reason for all three numbers and is asserted, not assumed: a window longer
/// than the ring would silently compare against the oldest sample it happened to
/// still hold and under-report growth.
pub const GROWTH_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Resident size at which a running process is worth a warning.
///
/// Protects against: a process that has grown far past anything measured healthy
/// continuing without comment until the machine is under pressure.
///
/// 2 GiB is 1.354x the highest peak M1 measured on a healthy 931-message session
/// (1,549,164 KiB) and 1.789x its median (1,198,872 KiB), so the ordinary
/// run-to-run spread cannot reach it. Deliberately *not* derived from G2's
/// ceiling: that ceiling constrains a five-run median, while this compares a
/// single live sample, and M1's own run 4 exceeded the ceiling while passing the
/// gate.
pub const WARNING_RSS_KIB: u64 = 2 * 1024 * 1024;

/// Resident size at which a running process needs attention now.
///
/// Protects against: a warning being the loudest thing said about a process that
/// is on its way to exhausting the machine.
///
/// 4 GiB is 2x [`WARNING_RSS_KIB`] and 2.71x M1's highest healthy peak.
pub const CRITICAL_RSS_KIB: u64 = 4 * 1024 * 1024;

/// Growth across [`GROWTH_WINDOW`] that is worth a warning.
///
/// Protects against: a slow leak that never reaches [`WARNING_RSS_KIB`] within a
/// session but would over a longer one.
///
/// 512 MiB is 42.7% of the 1,198,872 KiB one whole measured large session costs,
/// so half a session's worth of unexplained growth inside 15 minutes qualifies.
pub const WARNING_GROWTH_KIB: u64 = 512 * 1024;

/// Growth across [`GROWTH_WINDOW`] that needs attention now.
///
/// 2 GiB is 1.71x the cost of the entire measured 931-message session, so this is
/// growth no single legitimate session can explain.
pub const CRITICAL_GROWTH_KIB: u64 = 2 * 1024 * 1024;

/// Active sessions at which session count alone explains the size.
///
/// Protects against: attributing a large process to a leak when it is simply
/// holding many sessions. Set from the measured per-session cost: at 1,198,872 KiB
/// for one 931-message session, even a handful of concurrent sessions of that size
/// exceeds [`WARNING_RSS_KIB`], so 32 is already far past what the measured
/// workload implies and reaching it is itself the finding.
pub const WARNING_ACTIVE_SESSIONS: u32 = 32;

/// Active sessions at which the count is the whole story.
pub const CRITICAL_ACTIVE_SESSIONS: u32 = 128;

/// Share of resident bytes one region must hold before it is named the cause.
///
/// Protects against: naming a region as the cause when it merely happens to be
/// the larger half. A bare majority would also make [`Attribution::Unattributed`]
/// unreachable — anonymous and mapped bytes partition `VmRSS`, so at a 1/2 share
/// one of them always wins and the fourth level would be dead machinery.
///
/// Two thirds leaves a genuine middle where neither region explains the size,
/// which is information rather than a guess. Real profiles do land decisively:
/// this process's own split at startup is 2,320 KiB file-backed against 184 KiB
/// anonymous, 92.6% mapped, well past the share.
pub const DOMINANT_SHARE_NUMERATOR: u64 = 2;
/// Denominator of [`DOMINANT_SHARE_NUMERATOR`].
pub const DOMINANT_SHARE_DENOMINATOR: u64 = 3;

/// One resident-memory observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySample {
    /// Milliseconds since the ring was created.
    pub elapsed_ms: u64,
    /// Anonymous resident KiB: heap and thread stacks.
    pub anon_kib: u64,
    /// File-backed resident KiB: the binary's text and any mapped file.
    pub file_kib: u64,
    /// Shared-memory resident KiB.
    pub shmem_kib: u64,
    /// Sessions the process was holding when this was sampled.
    pub active_sessions: u32,
}

impl MemorySample {
    /// Total resident KiB, the sum Linux reports as `VmRSS`.
    #[must_use]
    pub const fn total_rss_kib(&self) -> u64 {
        self.anon_kib
            .saturating_add(self.file_kib)
            .saturating_add(self.shmem_kib)
    }
}

/// How severe an incident is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth looking at.
    Warning,
    /// Needs attention now.
    Critical,
}

impl Severity {
    /// The stable name used in log lines and assertions.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// Why the process is this large, in the order the levels are tried.
///
/// The order is the whole design. Each level is only reached when the ones above
/// it do not explain the size, so the answer is the *most specific* cause that
/// fits rather than whichever check happened to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    /// The process is holding many sessions. Tried first: it is the one cause
    /// that is not a defect, and reporting a leak here would send the reader
    /// looking for one that does not exist.
    SessionCount,
    /// Most resident bytes are file-backed or shared. Tried before the heap
    /// because file-backed growth is not something a heap fix would address —
    /// a mapped database snapshot, not a leak.
    MappedGrowth,
    /// Anonymous resident bytes dominate and the session count does not explain
    /// them. This is heap, thread stacks, and any pages the allocator has freed
    /// but not yet returned; the 1-second decay tuning makes the last of those
    /// short-lived, so it is reported as one cause rather than guessed apart.
    AnonymousHeap,
    /// Nothing above dominates. Reported as its own answer rather than folded
    /// into the heap, because "we do not know" and "it is the heap" send a reader
    /// to different places.
    Unattributed,
}

impl Attribution {
    /// The stable name used in log lines and assertions.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionCount => "session_count",
            Self::MappedGrowth => "mapped_growth",
            Self::AnonymousHeap => "anonymous_heap",
            Self::Unattributed => "unattributed",
        }
    }
}

/// One incident, with what triggered it and what it is attributed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryIncident {
    /// How severe.
    pub severity: Severity,
    /// The most specific cause that fits.
    pub attribution: Attribution,
    /// Resident KiB at the newest sample.
    pub total_rss_kib: u64,
    /// Growth in KiB across the window, zero when the process shrank.
    pub growth_kib: u64,
    /// How much of the window the ring actually covered.
    pub window: Duration,
    /// Sessions held at the newest sample.
    pub active_sessions: u32,
}

impl MemoryIncident {
    /// A single line carrying every field, for a sink that only takes text.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "memory.incident severity={} attribution={} rss_kib={} growth_kib={} \
             window_ms={} active_sessions={}",
            self.severity.as_str(),
            self.attribution.as_str(),
            self.total_rss_kib,
            self.growth_kib,
            self.window.as_millis(),
            self.active_sessions
        )
    }
}

/// The short-term resident-memory history, bounded by construction.
#[derive(Debug)]
pub struct MemoryRing {
    capacity: usize,
    samples: VecDeque<MemorySample>,
    observed: u64,
}

impl Default for MemoryRing {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryRing {
    /// An empty ring holding [`MEMORY_RING_SAMPLES`] samples.
    #[must_use]
    pub fn new() -> Self {
        Self {
            capacity: MEMORY_RING_SAMPLES,
            samples: VecDeque::with_capacity(MEMORY_RING_SAMPLES),
            observed: 0,
        }
    }

    /// Add one sample, dropping the oldest once the bound is reached.
    pub fn push(&mut self, sample: MemorySample) {
        self.observed += 1;
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// The retained samples, oldest first.
    #[must_use]
    pub fn samples(&self) -> Vec<MemorySample> {
        self.samples.iter().copied().collect()
    }

    /// How many samples were ever pushed, including ones the bound dropped.
    ///
    /// Kept separately from [`Self::samples`] because the bound drops records: a
    /// count derived from the retained ones would stop growing at the bound and a
    /// long-running process would look like it had just started.
    #[must_use]
    pub const fn observed(&self) -> u64 {
        self.observed
    }

    /// The newest sample.
    #[must_use]
    pub fn latest(&self) -> Option<MemorySample> {
        self.samples.back().copied()
    }

    /// The incident the retained history implies, if any.
    ///
    /// Growth is measured against the oldest sample still inside
    /// [`GROWTH_WINDOW`], so a ring that has not yet filled reports the growth it
    /// can actually see rather than none at all.
    #[must_use]
    pub fn incident(&self) -> Option<MemoryIncident> {
        let latest = self.latest()?;
        let window_start_ms = latest
            .elapsed_ms
            .saturating_sub(u64::try_from(GROWTH_WINDOW.as_millis()).unwrap_or(u64::MAX));
        let baseline = self
            .samples
            .iter()
            .find(|sample| sample.elapsed_ms >= window_start_ms)
            .copied()
            .unwrap_or(latest);
        let growth_kib = latest
            .total_rss_kib()
            .saturating_sub(baseline.total_rss_kib());
        let window = Duration::from_millis(latest.elapsed_ms.saturating_sub(baseline.elapsed_ms));
        let severity = severity(&latest, growth_kib)?;
        Some(MemoryIncident {
            severity,
            attribution: attribute(&latest),
            total_rss_kib: latest.total_rss_kib(),
            growth_kib,
            window,
            active_sessions: latest.active_sessions,
        })
    }
}

fn severity(latest: &MemorySample, growth_kib: u64) -> Option<Severity> {
    let total = latest.total_rss_kib();
    if total >= CRITICAL_RSS_KIB
        || growth_kib >= CRITICAL_GROWTH_KIB
        || latest.active_sessions >= CRITICAL_ACTIVE_SESSIONS
    {
        return Some(Severity::Critical);
    }
    if total >= WARNING_RSS_KIB
        || growth_kib >= WARNING_GROWTH_KIB
        || latest.active_sessions >= WARNING_ACTIVE_SESSIONS
    {
        return Some(Severity::Warning);
    }
    None
}

fn attribute(latest: &MemorySample) -> Attribution {
    if latest.active_sessions >= WARNING_ACTIVE_SESSIONS {
        return Attribution::SessionCount;
    }
    let mapped = latest.file_kib.saturating_add(latest.shmem_kib);
    let total = latest.total_rss_kib();
    if dominates(mapped, total) {
        return Attribution::MappedGrowth;
    }
    if dominates(latest.anon_kib, total) {
        return Attribution::AnonymousHeap;
    }
    Attribution::Unattributed
}

/// Whether `region` holds at least [`DOMINANT_SHARE_NUMERATOR`] of `total`.
///
/// Cross-multiplied rather than divided so the comparison is exact at every size;
/// a float ratio would make the boundary depend on rounding.
fn dominates(region: u64, total: u64) -> bool {
    total > 0
        && region.saturating_mul(DOMINANT_SHARE_DENOMINATOR)
            >= total.saturating_mul(DOMINANT_SHARE_NUMERATOR)
}

/// Report one incident through `tracing`.
///
/// `error` for critical and `warn` for warning, matching the watchdog's mapping so
/// one log can be read at one level without learning two conventions.
pub fn report(incident: &MemoryIncident) {
    TracingSink.report(incident);
}

/// How a sampler's findings leave the process.
///
/// A seam for the same reason [`crate::watchdog::WatchdogSink`] is one: the alert path
/// is the whole point of the sampler, and a test that called [`MemoryRing::incident`]
/// directly would prove the arithmetic while saying nothing about whether anything runs
/// it. Every level below was implemented and tested that way, and none of it was reachable
/// — there was no sampler at all.
pub trait MemorySink: Send + 'static {
    /// Record one incident. Must not block: it runs on the sampler thread.
    fn report(&self, incident: &MemoryIncident);

    /// Record that the process came back under every threshold.
    ///
    /// A default no-op because recovery is not a problem, but the transition is worth a
    /// line: without it a log shows a warning and then silence, which reads the same as
    /// a sampler that died.
    fn recovered(&self, _latest: &MemorySample) {}
}

/// The production sink: `tracing` at a level matching the incident's severity.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingSink;

impl MemorySink for TracingSink {
    fn report(&self, incident: &MemoryIncident) {
        match incident.severity {
            Severity::Critical => tracing::error!(target: "memory", "{}", incident.summary()),
            Severity::Warning => tracing::warn!(target: "memory", "{}", incident.summary()),
        }
    }

    fn recovered(&self, latest: &MemorySample) {
        tracing::warn!(
            target: "memory",
            "memory.recovered rss_kib={} active_sessions={}",
            latest.total_rss_kib(),
            latest.active_sessions
        );
    }
}

/// How many identical repeats are logged before the interval starts doubling.
///
/// One. An incident that persists is one fact, not one fact per sample: at
/// [`SAMPLE_EVERY`] an unattended process past 2 GiB would otherwise write a line every
/// two seconds for as long as it runs — 43,200 lines a day saying what the first one
/// said. That is its own defect, and it is the one that fills a disk while claiming to
/// diagnose memory. Matches [`crate::watchdog::STALL_BACKOFF_FACTOR`]'s reasoning.
pub const REPEAT_BACKOFF_FACTOR: u32 = 2;

/// The longest gap between repeats of an unchanged incident.
///
/// Ten minutes, so a process that has been over the threshold for hours still proves the
/// sampler is alive and still says so, without the log being mostly repetition.
pub const MAX_REPEAT_BACKOFF: Duration = Duration::from_secs(600);

/// A change worth reporting immediately, regardless of the backoff.
///
/// Severity or attribution moving is new information — `warning` becoming `critical`, or
/// growth switching from mapped files to the heap — so it resets the interval rather than
/// waiting it out. Growth alone does not: it changes on almost every sample.
const fn is_new_information(previous: &MemoryIncident, current: &MemoryIncident) -> bool {
    previous.severity as u8 != current.severity as u8
        || previous.attribution as u8 != current.attribution as u8
}

/// Where a sampler reads its resident split and its session count from.
///
/// A trait rather than a closure so a test can drive the sampler through synthetic growth
/// without waiting for a real process to leak, while production passes [`ProcSource`] and
/// exercises the same loop.
pub trait MemorySource: Send + 'static {
    /// The current sample, or `None` when the platform cannot report one.
    fn sample(&mut self, elapsed: Duration) -> Option<MemorySample>;
}

/// The production source: `/proc/self/status`, plus a caller-supplied session count.
pub struct ProcSource {
    sessions: Arc<AtomicU32>,
}

impl ProcSource {
    /// A source counting sessions through `sessions`.
    ///
    /// Shared rather than captured because the count changes while the sampler runs, and
    /// a figure read once at startup would attribute a 40-session process to whatever it
    /// had at launch — turning [`Attribution::SessionCount`] into a permanent wrong answer.
    #[must_use]
    pub const fn new(sessions: Arc<AtomicU32>) -> Self {
        Self { sessions }
    }
}

impl MemorySource for ProcSource {
    fn sample(&mut self, elapsed: Duration) -> Option<MemorySample> {
        observe(elapsed, self.sessions.load(Ordering::Relaxed))
    }
}

/// The process-wide count of sessions currently in flight.
///
/// A process global, and deliberately: [`Attribution::SessionCount`] is the level that
/// tells "many sessions, each reasonably sized" apart from "one session leaking", and it
/// needs the count as it is *now*. The sampler starts before any command has built a
/// session registry, so threading a handle from the entry point down to whatever owns the
/// sessions would mean either wiring it through every command or reading the count once at
/// launch and being permanently wrong. A counter the owner increments and the sampler reads
/// costs one atomic and cannot go stale.
///
/// Guard with [`SessionCount`] rather than by hand, so an early return or a panic cannot
/// leave the count above zero and make a healthy process look like it is hoarding sessions.
pub fn active_sessions() -> &'static Arc<AtomicU32> {
    static ACTIVE: OnceLock<Arc<AtomicU32>> = OnceLock::new();
    ACTIVE.get_or_init(|| Arc::new(AtomicU32::new(0)))
}

/// RAII marker that one session is in flight.
#[derive(Debug)]
pub struct SessionCount;

impl SessionCount {
    /// Count one session until the returned guard drops.
    #[must_use]
    pub fn enter() -> Self {
        active_sessions().fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for SessionCount {
    fn drop(&mut self) {
        // Saturating rather than wrapping: a decrement that underflowed would report
        // 4 billion sessions and send the attribution to the wrong level entirely.
        let _previous =
            active_sessions().fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_sub(1))
            });
    }
}

/// A running memory sampler: a thread, a bounded ring, and a rate-limited alert path.
///
/// # Why this exists as a type
///
/// Every level of [`MemoryRing::incident`] was implemented and unit-tested, and a
/// repo-wide search found no production construction of a ring and no production call to
/// [`observe`] or [`report`]. The alert could not fire however far resident memory grew.
/// This is the missing driver, and it is a thread rather than a `tokio::spawn` for the
/// reason the watchdog's is: it must keep sampling while the runtime's workers are all
/// blocked, which is exactly the state worth a report.
pub struct MemorySampler {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl MemorySampler {
    /// Start sampling `/proc` every [`SAMPLE_EVERY`], reporting through [`TracingSink`].
    #[must_use]
    pub fn spawn(sessions: Arc<AtomicU32>) -> Self {
        Self::spawn_with(SAMPLE_EVERY, ProcSource::new(sessions), TracingSink)
    }

    /// Start sampling `source` every `every`, reporting through `sink`.
    #[must_use]
    pub fn spawn_with(every: Duration, source: impl MemorySource, sink: impl MemorySink) -> Self {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let worker = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("zuno-memory-sampler".to_owned())
            .spawn(move || sample_until_stopped(&worker, every, source, sink))
            .ok();
        Self { stop, thread }
    }

    /// Stop sampling and wait for the thread.
    ///
    /// Joined rather than detached so a caller that shuts logging down next cannot race a
    /// report into a closed writer.
    pub fn shutdown(mut self) {
        if let Ok(mut stop) = self.stop.0.lock() {
            *stop = true;
        }
        self.stop.1.notify_all();
        if let Some(thread) = self.thread.take() {
            let _joined = thread.join();
        }
    }
}

impl Drop for MemorySampler {
    fn drop(&mut self) {
        if let Ok(mut stop) = self.stop.0.lock() {
            *stop = true;
        }
        self.stop.1.notify_all();
    }
}

fn sample_until_stopped(
    stop: &Arc<(Mutex<bool>, Condvar)>,
    every: Duration,
    mut source: impl MemorySource,
    sink: impl MemorySink,
) {
    let started = Instant::now();
    let mut ring = MemoryRing::new();
    let mut reported: Option<MemoryIncident> = None;
    let mut next_repeat = Duration::ZERO;
    let mut backoff = every;

    loop {
        if park(stop, every) {
            return;
        }
        let now = started.elapsed();
        let Some(sample) = source.sample(now) else {
            // Off Linux there is no split to read, so there is nothing to say. Returning
            // rather than spinning: the answer will not change.
            return;
        };
        ring.push(sample);
        match ring.incident() {
            Some(incident) => {
                let escalated = reported
                    .as_ref()
                    .is_none_or(|previous| is_new_information(previous, &incident));
                if escalated {
                    backoff = every;
                    next_repeat = Duration::ZERO;
                }
                if now >= next_repeat {
                    sink.report(&incident);
                    next_repeat = now.saturating_add(backoff);
                    backoff = backoff
                        .saturating_mul(REPEAT_BACKOFF_FACTOR)
                        .min(MAX_REPEAT_BACKOFF);
                }
                reported = Some(incident);
            }
            None => {
                if reported.take().is_some() {
                    sink.recovered(&sample);
                    backoff = every;
                    next_repeat = Duration::ZERO;
                }
            }
        }
    }
}

/// Park for at most `timeout`. `true` means shutdown was requested.
fn park(stop: &Arc<(Mutex<bool>, Condvar)>, timeout: Duration) -> bool {
    let Ok(guard) = stop.0.lock() else {
        return true;
    };
    if *guard {
        return true;
    }
    match stop.1.wait_timeout(guard, timeout) {
        Ok((guard, _)) => *guard,
        Err(_) => true,
    }
}

/// One `Key:\t<value> kB` field of a `/proc/self/status` snapshot, in KiB.
///
/// Shared with the tests that check the split against `VmRSS`, so those read the
/// kernel's own total through the *same* scraper the shipped path reads the three
/// parts through. A second scraper written for a test could agree with this one
/// while both misread the file, and the test would still be green.
#[cfg(target_os = "linux")]
fn status_field_kib(status: &str, key: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
}

/// The resident split carried by **one** `/proc/self/status` snapshot.
///
/// Split out of [`observe`] so the sample comes from a single read, and so a test
/// can compare the anon/file/shmem sum against the `VmRSS` of *that same
/// snapshot* — exactly, with no tolerance. The four figures in one snapshot agree
/// to the KiB because the kernel prints `VmRSS` as `anon + file + shmem` from a
/// single read of the three counters (`task_mem` in `fs/proc/task_mmu.c`), and
/// `/proc/self/status` is rendered by one `show` call; measured here at 400 reads
/// under allocation churn with zero mismatches.
///
/// Two reads do not agree: the process's own memory moves in between. An earlier
/// form of the test below read the file once inside [`observe`] and once for
/// `VmRSS` and allowed 5% drift, which passed locally and failed on a loaded CI
/// runner — it was measuring the scheduler, not the accounting.
#[cfg(target_os = "linux")]
fn parse_status(status: &str, elapsed: Duration, active_sessions: u32) -> Option<MemorySample> {
    Some(MemorySample {
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        anon_kib: status_field_kib(status, "RssAnon:")?,
        file_kib: status_field_kib(status, "RssFile:")?,
        // Absent before Linux 4.5 and zero for most processes, so a missing field
        // is not a failed read.
        shmem_kib: status_field_kib(status, "RssShmem:").unwrap_or(0),
        active_sessions,
    })
}

/// Read the current resident split from Linux, or `None` off Linux.
#[cfg(target_os = "linux")]
#[must_use]
pub fn observe(elapsed: Duration, active_sessions: u32) -> Option<MemorySample> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_status(&status, elapsed, active_sessions)
}

/// Read the current resident split from Linux, or `None` off Linux.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn observe(_elapsed: Duration, _active_sessions: u32) -> Option<MemorySample> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(elapsed_ms: u64, anon_kib: u64, file_kib: u64, active_sessions: u32) -> MemorySample {
        MemorySample {
            elapsed_ms,
            anon_kib,
            file_kib,
            shmem_kib: 0,
            active_sessions,
        }
    }

    /// The three constants are one decision; a window past the ring under-reports.
    #[test]
    fn ring_covers_the_growth_window() {
        let needed = GROWTH_WINDOW.as_secs() / SAMPLE_EVERY.as_secs();
        assert!(
            needed < MEMORY_RING_SAMPLES as u64,
            "a {GROWTH_WINDOW:?} window at {SAMPLE_EVERY:?} needs {needed} samples, more than \
             the {MEMORY_RING_SAMPLES}-sample ring holds, so growth would be measured against \
             whatever sample happened to survive"
        );
    }

    #[test]
    fn a_healthy_measured_session_raises_no_incident() {
        // M1's tuned-jemalloc W-real median and its highest of five runs.
        let mut ring = MemoryRing::new();
        ring.push(sample(0, 1_100_000, 98_872, 1));
        ring.push(sample(600_000, 1_400_000, 149_164, 1));

        assert_eq!(
            ring.incident(),
            None,
            "a peak inside M1's measured spread was reported as an incident, so every \
             ordinary large session would alert"
        );
    }

    /// The reference's 1 GiB warning against this project's measured median.
    #[test]
    fn the_reference_warning_threshold_would_have_fired_on_a_healthy_session() {
        let measured_median_kib = 1_198_872_u64;
        let reference_warning_kib = 1024 * 1024;
        assert!(
            measured_median_kib > reference_warning_kib,
            "the premise of this project's own thresholds is that the reference's 1 GiB \
             warning sits below the measured healthy median"
        );
        assert!(
            measured_median_kib < WARNING_RSS_KIB,
            "the shipped warning threshold sits below the measured healthy median, so it \
             would fire on every large session"
        );
    }

    #[test]
    fn many_sessions_are_attributed_to_the_session_count_not_to_the_heap() {
        let mut ring = MemoryRing::new();
        ring.push(sample(0, 3_000_000, 40_000, WARNING_ACTIVE_SESSIONS));

        let incident = ring.incident().expect("32 sessions crosses the warning");
        assert_eq!(
            incident.attribution,
            Attribution::SessionCount,
            "a process holding {} sessions was blamed on {}, sending a reader to look for a \
             leak that is not there",
            incident.active_sessions,
            incident.attribution.as_str()
        );
        assert_eq!(incident.severity, Severity::Warning);
    }

    #[test]
    fn file_backed_bytes_are_attributed_to_mapping_not_to_the_heap() {
        let mut ring = MemoryRing::new();
        ring.push(sample(0, 900_000, 2_500_000, 1));

        let incident = ring.incident().expect("3.3 GiB crosses the warning");
        assert_eq!(
            incident.attribution,
            Attribution::MappedGrowth,
            "a mostly file-backed process was blamed on {}, which no heap fix would address",
            incident.attribution.as_str()
        );
    }

    #[test]
    fn anonymous_bytes_with_few_sessions_are_attributed_to_the_heap() {
        let mut ring = MemoryRing::new();
        ring.push(sample(0, 3_000_000, 100_000, 2));

        let incident = ring.incident().expect("3 GiB crosses the warning");
        assert_eq!(incident.attribution, Attribution::AnonymousHeap);
        assert_eq!(incident.severity, Severity::Warning);
    }

    #[test]
    fn an_even_split_is_reported_as_unattributed_rather_than_guessed() {
        let half = WARNING_RSS_KIB / 2;
        let mut ring = MemoryRing::new();
        ring.push(sample(0, half, half, 1));

        let incident = ring.incident().expect("the total crosses the warning");
        assert_eq!(
            incident.attribution,
            Attribution::Unattributed,
            "a 50/50 split was blamed on {}, which would send a reader to one region on the \
             strength of a coin flip",
            incident.attribution.as_str()
        );
    }

    /// All four levels must be reachable, or the fourth is decoration.
    #[test]
    fn every_attribution_level_is_reachable_from_a_real_split() {
        let reached = [
            sample(0, 1_000, 1_000, CRITICAL_ACTIVE_SESSIONS),
            sample(0, 1_000, 9_000, 1),
            sample(0, 9_000, 1_000, 1),
            sample(0, 5_000, 5_000, 1),
        ]
        .map(|sample| attribute(&sample));

        assert_eq!(
            reached,
            [
                Attribution::SessionCount,
                Attribution::MappedGrowth,
                Attribution::AnonymousHeap,
                Attribution::Unattributed,
            ]
        );
    }

    /// The measured split of this very process, so the share is not merely plausible.
    #[test]
    fn this_process_startup_split_lands_decisively_on_mapping() {
        let measured = sample(0, 184, 2_320, 1);
        assert_eq!(attribute(&measured), Attribution::MappedGrowth);
    }

    #[test]
    fn growth_inside_the_window_escalates_even_below_the_size_threshold() {
        let mut ring = MemoryRing::new();
        ring.push(sample(0, 200_000, 50_000, 1));
        ring.push(sample(600_000, 200_000 + CRITICAL_GROWTH_KIB, 50_000, 1));

        let incident = ring.incident().expect("growth alone must escalate");
        assert!(
            incident.total_rss_kib < CRITICAL_RSS_KIB,
            "the fixture crossed the size threshold, so it does not test growth"
        );
        assert_eq!(incident.severity, Severity::Critical);
        assert_eq!(incident.growth_kib, CRITICAL_GROWTH_KIB);
        assert_eq!(incident.window, Duration::from_secs(600));
    }

    #[test]
    fn growth_older_than_the_window_is_not_counted() {
        let window_ms = u64::try_from(GROWTH_WINDOW.as_millis()).expect("window fits in u64");
        let mut ring = MemoryRing::new();
        ring.push(sample(0, 100_000, 50_000, 1));
        ring.push(sample(
            window_ms + 60_000,
            100_000 + WARNING_GROWTH_KIB,
            50_000,
            1,
        ));
        ring.push(sample(
            window_ms + 120_000,
            100_000 + WARNING_GROWTH_KIB,
            50_000,
            1,
        ));

        assert_eq!(
            ring.incident(),
            None,
            "growth from before the {GROWTH_WINDOW:?} window was counted, so a process that \
             grew once at startup would alert forever"
        );
    }

    #[test]
    fn the_ring_never_holds_more_than_its_bound() {
        let mut ring = MemoryRing::new();
        let overrun = MEMORY_RING_SAMPLES * 3;
        for index in 0..overrun {
            ring.push(sample(index as u64 * 2_000, 100_000, 50_000, 1));
        }

        let retained = ring.samples();
        assert_eq!(
            retained.len(),
            MEMORY_RING_SAMPLES,
            "the ring grew to {} samples against a {MEMORY_RING_SAMPLES}-sample bound",
            retained.len()
        );
        assert_eq!(
            ring.observed(),
            overrun as u64,
            "the observation count stopped at the bound, so a week-old process would look new"
        );
        assert_eq!(
            retained.first().map(|sample| sample.elapsed_ms),
            Some((overrun - MEMORY_RING_SAMPLES) as u64 * 2_000),
            "the bound dropped the newest samples instead of the oldest"
        );
    }

    #[test]
    fn a_retained_sample_is_fixed_size_so_the_sample_count_is_the_byte_bound() {
        assert_eq!(std::mem::size_of::<MemorySample>(), 40);
        assert_eq!(
            MEMORY_RING_SAMPLES * std::mem::size_of::<MemorySample>(),
            20_480
        );
    }

    #[test]
    fn an_incident_summary_names_every_field_a_reader_needs() {
        let mut ring = MemoryRing::new();
        ring.push(sample(0, CRITICAL_RSS_KIB, 0, 3));

        let incident = ring.incident().expect("the size crosses critical");
        assert_eq!(
            incident.summary(),
            "memory.incident severity=critical attribution=anonymous_heap rss_kib=4194304 \
             growth_kib=0 window_ms=0 active_sessions=3"
        );
    }

    /// A verbatim `Vm*`/`Rss*` block from a real `/proc/<pid>/status`.
    ///
    /// `RssShmem` is non-zero on purpose. It is zero for most processes, so a
    /// snapshot taken from this test's own process would let a sum that drops
    /// shmem pass. The three parts are also pairwise distinct, so a sum that
    /// preserves the total by swapping two fields is still caught. `VmHWM` and
    /// `VmData` are kept because they are the neighbours a scraper could misread:
    /// same units, immediately above and below `VmRSS`.
    #[cfg(target_os = "linux")]
    const CAPTURED_STATUS: &str = "\
VmPeak:\t  770196 kB
VmSize:\t  245908 kB
VmLck:\t       0 kB
VmPin:\t       0 kB
VmHWM:\t   11856 kB
VmRSS:\t   10904 kB
RssAnon:\t    2768 kB
RssFile:\t    7448 kB
RssShmem:\t     688 kB
VmData:\t   19136 kB
VmStk:\t     132 kB
VmExe:\t      44 kB
VmLib:\t   15688 kB
VmPTE:\t     124 kB
VmSwap:\t       0 kB
";

    /// The split is the same accounting as `VmRSS`, at exactly known values.
    ///
    /// Exact and against a fixture, so nothing can move between the two figures
    /// being compared. The per-field assertion is not redundant with the sum: a
    /// parse that swapped `RssFile` for `RssShmem` would keep the total intact and
    /// still send the attribution to the wrong level.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_parsed_split_sums_to_the_vm_rss_of_the_same_snapshot() {
        let sample = parse_status(CAPTURED_STATUS, Duration::from_secs(1), 0)
            .expect("the captured snapshot publishes the split");

        assert_eq!(
            (sample.anon_kib, sample.file_kib, sample.shmem_kib),
            (2_768, 7_448, 688),
            "the three fields were read off the wrong lines of the snapshot"
        );
        let vm_rss = status_field_kib(CAPTURED_STATUS, "VmRSS:").expect("VmRSS is published");
        assert_eq!(
            vm_rss, 10_904,
            "the scraper read {vm_rss} for the fixture's VmRSS, so it is not reading VmRSS"
        );
        assert_eq!(
            sample.total_rss_kib(),
            vm_rss,
            "the anon/file/shmem split summed to {} against the VmRSS of {vm_rss} published in \
             the same snapshot, so it is not the same accounting",
            sample.total_rss_kib()
        );
    }

    /// The same contract against the kernel this build actually runs on.
    ///
    /// One read, both figures derived from it, so the comparison is exact. The
    /// earlier form read `/proc/self/status` twice — once inside [`observe`], once
    /// for `VmRSS` — and allowed 5% drift; on a loaded runner the process's own
    /// memory moves between the reads and the drift exceeds the bound, so it
    /// failed in CI while passing locally. Reading once removes the race rather
    /// than lowering its odds.
    #[cfg(target_os = "linux")]
    #[test]
    fn one_live_snapshot_of_this_process_splits_exactly_into_its_vm_rss() {
        let status =
            std::fs::read_to_string("/proc/self/status").expect("/proc/self/status is readable");
        let sample =
            parse_status(&status, Duration::from_secs(1), 0).expect("Linux publishes the split");
        let vm_rss = status_field_kib(&status, "VmRSS:").expect("VmRSS is published");

        assert!(
            sample.total_rss_kib() > 0,
            "a running process reported no resident memory, so every field parsed as zero"
        );
        assert_eq!(
            sample.total_rss_kib(),
            vm_rss,
            "this kernel's anon/file/shmem summed to {} against the VmRSS of {vm_rss} it \
             published in the same snapshot, so the split does not partition VmRSS",
            sample.total_rss_kib()
        );
    }

    /// [`observe`] reaches the real file and carries its caller's arguments.
    ///
    /// The two tests above call [`parse_status`] directly, so neither would notice
    /// `observe` reading the wrong path, dropping a field on the way through, or
    /// substituting its own session count. The accounting contract is not
    /// re-checked here: that needs two figures from one snapshot and `observe`
    /// returns only the sample, which is what made the earlier test read twice.
    #[cfg(target_os = "linux")]
    #[test]
    fn observe_reads_this_process_through_the_shipped_entry_point() {
        let sample = observe(Duration::from_secs(1), 7).expect("Linux publishes the split");

        assert!(
            sample.anon_kib > 0,
            "this process's heap and thread stacks read as zero anonymous KiB, so observe did \
             not reach the real file"
        );
        assert!(sample.total_rss_kib() > 0);
        assert_eq!(
            sample.elapsed_ms, 1_000,
            "the elapsed argument was not carried, so every sample would land at one instant"
        );
        assert_eq!(
            sample.active_sessions, 7,
            "the session count was not carried, so attribution would judge whatever observe \
             substituted"
        );
    }
}
