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
use std::time::Duration;

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
    match incident.severity {
        Severity::Critical => tracing::error!(target: "memory", "{}", incident.summary()),
        Severity::Warning => tracing::warn!(target: "memory", "{}", incident.summary()),
    }
}

/// Read the current resident split from Linux, or `None` off Linux.
#[cfg(target_os = "linux")]
#[must_use]
pub fn observe(elapsed: Duration, active_sessions: u32) -> Option<MemorySample> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    };
    Some(MemorySample {
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        anon_kib: field("RssAnon:")?,
        file_kib: field("RssFile:")?,
        shmem_kib: field("RssShmem:").unwrap_or(0),
        active_sessions,
    })
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

    #[cfg(target_os = "linux")]
    #[test]
    fn observing_this_process_returns_a_split_that_sums_to_its_resident_size() {
        let sample = observe(Duration::from_secs(1), 0).expect("Linux publishes the split");
        assert!(sample.total_rss_kib() > 0);
        assert_eq!(sample.elapsed_ms, 1_000);

        let vm_rss = std::fs::read_to_string("/proc/self/status")
            .expect("status is readable")
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .expect("VmRSS is published");
        let drift = vm_rss.abs_diff(sample.total_rss_kib());
        assert!(
            drift * 20 <= vm_rss,
            "the anon/file/shmem split summed to {} against a VmRSS of {vm_rss}, so it is not \
             the same accounting",
            sample.total_rss_kib()
        );
    }
}
