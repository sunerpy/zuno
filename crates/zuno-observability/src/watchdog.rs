//! Liveness watchdog: an independent OS thread that reports a stalled process.
//!
//! # Why this is not a task on the async runtime
//!
//! The failure this exists to describe is "the process stopped making progress",
//! and the most common cause of that on a Tokio program is the runtime itself
//! being wedged — a blocking call on a worker thread, a lock held across an
//! `await`, a deadlocked `futex`. A watchdog scheduled *by* that runtime cannot
//! run in exactly the case it is needed, so this one owns a plain
//! [`std::thread`] and reads only atomics and `/proc`.
//!
//! # Why a missing heartbeat alone is not a stall
//!
//! A CLI waiting for the user to type is silent and healthy. Reporting on
//! silence alone would make "sitting at a prompt" indistinguishable from "hung
//! mid-turn", so the watchdog only considers a missing heartbeat a stall while at
//! least one [`WorkGuard`] is alive. The guard is RAII: the busy count cannot be
//! left raised by an early `return`, a `?`, or a panic.
//!
//! # Bounded waits only
//!
//! Every wait in this module carries a deadline. The watchdog thread parks on
//! [`Condvar::wait_timeout`] for at most [`CHECK_EVERY`], so shutdown is
//! immediate when signalled and bounded when not, and [`Watchdog::shutdown`]
//! joins the thread rather than detaching it. An unbounded wait here would make
//! the liveness reporter the thing that hangs — see the clipboard child that once
//! blocked the UI lock indefinitely and was fixed with a bounded mailbox.
//!
//! ```no_run
//! # fn main() {
//! use zuno_observability::watchdog::{Watchdog, WatchdogConfig};
//!
//! let watchdog = Watchdog::spawn(WatchdogConfig::default());
//! let turn = watchdog.phase("turn.provider_request");
//! {
//!     // While this guard lives, silence longer than the stall threshold is a
//!     // stall. Dropping it makes the same silence healthy again.
//!     let _work = watchdog.begin_work(turn);
//!     watchdog.beat(turn);
//! }
//! watchdog.shutdown();
//! # }
//! ```

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long a *busy* process may go without a heartbeat before it is a stall.
///
/// Protects against: a turn that is still holding a [`WorkGuard`] but has stopped
/// making progress — a deadlocked `futex`, a blocking syscall on a runtime worker,
/// an infinite loop — being noticed only when a human eventually complains.
///
/// 90s sits deliberately **below** the 120s of G4's frozen
/// `g4_progress_timeout_seconds`, so a stalled turn is described in the log
/// before the gate that fails the build on it trips. Raising this above 120s
/// would invert that order and leave every G4 failure unexplained.
pub const STALL_AFTER: Duration = Duration::from_secs(90);

/// How often the watchdog thread wakes to compare the clock against the beat.
///
/// Protects against: a stall being reported so late that the surrounding log
/// context has already scrolled away. It also bounds shutdown latency, because
/// this is the longest the thread can be parked before it observes the stop
/// flag. 5s keeps the idle cost to one wake per 5s while holding stall
/// attribution to within one check of [`STALL_AFTER`].
pub const CHECK_EVERY: Duration = Duration::from_secs(5);

/// How often a healthy watchdog says so.
///
/// Protects against: silence being ambiguous between "nothing went wrong" and
/// "the watchdog thread itself died". Without a positive liveness line there is
/// no way to tell those apart after the fact. 300s is long enough that the line
/// is not noise in a long session and short enough that a gap in it is visible.
pub const ALIVE_EVERY: Duration = Duration::from_secs(300);

/// Upper bound on per-thread rows in one stall report.
///
/// Protects against: a process with hundreds of threads turning a single stall
/// into a log flood that pushes the useful context out of the file.
pub const MAX_THREADS_DUMPED: usize = 48;

/// Multiplier applied to the reporting interval while a stall persists.
///
/// Protects against: a stall that lasts an hour emitting 720 identical reports at
/// [`CHECK_EVERY`]. The first report is immediate; each subsequent one waits
/// twice as long as the last, up to [`MAX_STALL_BACKOFF`].
pub const STALL_BACKOFF_FACTOR: u32 = 2;

/// Ceiling on the exponential backoff between repeated stall reports.
///
/// Protects against: backoff growing without limit and turning a stall that
/// *changed phase* into something the log never mentions again.
pub const MAX_STALL_BACKOFF: Duration = Duration::from_secs(600);

/// An interned phase label.
///
/// The whole point of interning is that [`Watchdog::beat`] is on the hot path of
/// every turn and must not allocate, lock, or copy a string. Registration pays
/// the lock once; a beat writes two relaxed atomics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phase(usize);

impl Phase {
    /// The label registered for this phase, or `"<unregistered>"`.
    #[must_use]
    pub fn label(self) -> &'static str {
        phase_registry()
            .lock()
            .map(|labels| labels.get(self.0).copied().unwrap_or(UNREGISTERED))
            .unwrap_or(UNREGISTERED)
    }
}

/// The phase a process reports before anything has beaten.
pub const UNSTARTED: &str = "<unstarted>";
/// The label a [`Phase`] resolves to when its index is not in the registry.
const UNREGISTERED: &str = "<unregistered>";

/// Process-wide interning table. Written only by [`Watchdog::phase`].
fn phase_registry() -> &'static Mutex<Vec<&'static str>> {
    static REGISTRY: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(vec![UNSTARTED]))
}

/// Tunable thresholds, defaulting to the frozen constants above.
///
/// The fields exist so the behaviour can be *driven* in a test rather than
/// asserted about: a watchdog whose stall path never fires under test is
/// untested machinery. Production callers use [`WatchdogConfig::default`], and
/// `watchdog_defaults_are_the_frozen_constants` pins that they get the frozen
/// values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogConfig {
    /// See [`STALL_AFTER`].
    pub stall_after: Duration,
    /// See [`CHECK_EVERY`].
    pub check_every: Duration,
    /// See [`ALIVE_EVERY`].
    pub alive_every: Duration,
    /// See [`MAX_THREADS_DUMPED`].
    pub max_threads_dumped: usize,
    /// See [`MAX_STALL_BACKOFF`].
    pub max_stall_backoff: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            stall_after: STALL_AFTER,
            check_every: CHECK_EVERY,
            alive_every: ALIVE_EVERY,
            max_threads_dumped: MAX_THREADS_DUMPED,
            max_stall_backoff: MAX_STALL_BACKOFF,
        }
    }
}

/// What the watchdog observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogEvent {
    /// A busy process went [`WatchdogConfig::stall_after`] without a heartbeat.
    Stalled,
    /// A previously stalled process beat again.
    Recovered,
    /// Routine confirmation that the watchdog thread is still running.
    Alive,
}

impl WatchdogEvent {
    /// The stable name used in log lines and assertions.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stalled => "watchdog.stalled",
            Self::Recovered => "watchdog.recovered",
            Self::Alive => "watchdog.alive",
        }
    }
}

/// One observation, with everything needed to attribute it.
///
/// The report is deliberately *not* an action: a watchdog that killed the process
/// would destroy the state a human needs to diagnose the stall. Every field here
/// exists to distinguish causes that look identical from the outside — a futex
/// deadlock from a busy loop from a blocked syscall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogReport {
    /// Which observation this is.
    pub event: WatchdogEvent,
    /// Interned label of the phase that last beat.
    pub phase: &'static str,
    /// How long since the last heartbeat.
    pub since_beat: Duration,
    /// How many [`WorkGuard`]s were alive.
    pub busy: usize,
    /// Resident set size in KiB, when `/proc` could be read.
    pub rss_kib: Option<u64>,
    /// Live thread count, when `/proc` could be read.
    pub threads: Option<usize>,
    /// Open file descriptors, when `/proc` could be read.
    pub open_fds: Option<usize>,
    /// `tid:name:state:wchan` rows, capped at
    /// [`WatchdogConfig::max_threads_dumped`].
    pub thread_rows: Vec<String>,
}

impl WatchdogReport {
    /// A single line carrying every field, for a sink that only takes text.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "{} phase={} since_beat_ms={} busy={}",
            self.event.as_str(),
            self.phase,
            self.since_beat.as_millis(),
            self.busy
        );
        if let Some(rss) = self.rss_kib {
            let _ = write!(out, " rss_kib={rss}");
        }
        if let Some(threads) = self.threads {
            let _ = write!(out, " threads={threads}");
        }
        if let Some(fds) = self.open_fds {
            let _ = write!(out, " open_fds={fds}");
        }
        if !self.thread_rows.is_empty() {
            let _ = write!(out, " thread_rows=[{}]", self.thread_rows.join(","));
        }
        out
    }
}

/// Where reports go.
///
/// Injectable so a test can observe the stall path directly. Scraping the log
/// file would test the subscriber instead of the watchdog, and would make the
/// assertion depend on a global logging init that other tests share.
pub trait WatchdogSink: Send + Sync + 'static {
    /// Record one observation. Must not block: it runs on the watchdog thread.
    fn report(&self, report: &WatchdogReport);
}

/// The production sink: `tracing` at a level matching the event's severity.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingSink;

impl WatchdogSink for TracingSink {
    fn report(&self, report: &WatchdogReport) {
        match report.event {
            WatchdogEvent::Stalled => tracing::error!(target: "watchdog", "{}", report.summary()),
            WatchdogEvent::Recovered => tracing::warn!(target: "watchdog", "{}", report.summary()),
            WatchdogEvent::Alive => tracing::debug!(target: "watchdog", "{}", report.summary()),
        }
    }
}

/// Atomics the hot path touches, plus the stop signal the thread parks on.
#[derive(Debug)]
struct State {
    started: Instant,
    /// Nanoseconds since `started` at the last beat.
    last_beat_nanos: AtomicU64,
    /// Interned index of the phase that beat last.
    phase: AtomicUsize,
    /// Live [`WorkGuard`] count. Zero means silence is healthy.
    busy: AtomicUsize,
    /// `true` once shutdown was requested; guarded by the condvar below.
    stop: Mutex<bool>,
    wake: Condvar,
}

impl State {
    fn elapsed_nanos(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// RAII marker that the process is doing work a stall would interrupt.
///
/// Holding one is what turns silence into a stall. Dropping one — including on an
/// early return or a panic — makes the same silence healthy, which is why the
/// busy count is never incremented and decremented by hand.
#[derive(Debug)]
pub struct WorkGuard {
    state: Arc<State>,
}

impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.state.busy.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Handle to a running watchdog thread.
#[derive(Debug)]
pub struct Watchdog {
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    /// Start a watchdog reporting through [`TracingSink`].
    #[must_use]
    pub fn spawn(config: WatchdogConfig) -> Self {
        Self::spawn_with_sink(config, TracingSink)
    }

    /// Start a watchdog reporting through `sink`.
    #[must_use]
    pub fn spawn_with_sink(config: WatchdogConfig, sink: impl WatchdogSink) -> Self {
        let state = Arc::new(State {
            started: Instant::now(),
            last_beat_nanos: AtomicU64::new(0),
            phase: AtomicUsize::new(0),
            busy: AtomicUsize::new(0),
            stop: Mutex::new(false),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&state);
        let thread = std::thread::Builder::new()
            .name("zuno-watchdog".to_owned())
            .spawn(move || run(&worker, config, &sink))
            .ok();
        Self { state, thread }
    }

    /// Intern a phase label once, so [`Self::beat`] stays allocation-free.
    #[must_use]
    pub fn phase(&self, label: &'static str) -> Phase {
        let Ok(mut labels) = phase_registry().lock() else {
            return Phase(0);
        };
        if let Some(index) = labels.iter().position(|known| *known == label) {
            return Phase(index);
        }
        labels.push(label);
        Phase(labels.len() - 1)
    }

    /// Record progress. Two relaxed atomic stores; no allocation, no lock.
    pub fn beat(&self, phase: Phase) {
        self.state.phase.store(phase.0, Ordering::Relaxed);
        self.state
            .last_beat_nanos
            .store(self.state.elapsed_nanos(), Ordering::Relaxed);
    }

    /// Mark the start of work whose silence is a stall, and beat once.
    #[must_use]
    pub fn begin_work(&self, phase: Phase) -> WorkGuard {
        self.state.busy.fetch_add(1, Ordering::Relaxed);
        self.beat(phase);
        WorkGuard {
            state: Arc::clone(&self.state),
        }
    }

    /// How many [`WorkGuard`]s are alive.
    #[must_use]
    pub fn busy(&self) -> usize {
        self.state.busy.load(Ordering::Relaxed)
    }

    /// Stop the thread and wait for it, bounded by one [`CHECK_EVERY`] park.
    ///
    /// The stop flag is set *and* the condvar notified, so the thread does not
    /// have to finish its current park before observing it. The join is
    /// deliberate: detaching would let the thread outlive the sink it reports to.
    pub fn shutdown(mut self) {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn request_stop(&self) {
        if let Ok(mut stop) = self.state.stop.lock() {
            *stop = true;
        }
        self.state.wake.notify_all();
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        // A dropped handle must not leave a thread reporting into a sink whose
        // owner is gone. `shutdown` is the documented path; this is the net.
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The watchdog thread body.
fn run(state: &Arc<State>, config: WatchdogConfig, sink: &impl WatchdogSink) {
    let mut stalled = false;
    let mut next_stall_report = Duration::ZERO;
    let mut stall_backoff = config.check_every;
    let mut last_alive = Duration::ZERO;

    loop {
        if park(state, config.check_every) {
            return;
        }
        let now = Duration::from_nanos(state.elapsed_nanos());
        let since_beat = now.saturating_sub(Duration::from_nanos(
            state.last_beat_nanos.load(Ordering::Relaxed),
        ));
        let busy = state.busy.load(Ordering::Relaxed);

        // The BUSY gate: only work in flight turns silence into a stall.
        if busy > 0 && since_beat >= config.stall_after {
            if !stalled {
                stalled = true;
                stall_backoff = config.check_every;
                next_stall_report = Duration::ZERO;
            }
            if now >= next_stall_report {
                sink.report(&observe(
                    state,
                    WatchdogEvent::Stalled,
                    since_beat,
                    busy,
                    &config,
                ));
                next_stall_report = now.saturating_add(stall_backoff);
                stall_backoff = stall_backoff
                    .saturating_mul(STALL_BACKOFF_FACTOR)
                    .min(config.max_stall_backoff);
            }
        } else if stalled {
            stalled = false;
            sink.report(&observe(
                state,
                WatchdogEvent::Recovered,
                since_beat,
                busy,
                &config,
            ));
        }

        if now.saturating_sub(last_alive) >= config.alive_every {
            last_alive = now;
            sink.report(&observe(
                state,
                WatchdogEvent::Alive,
                since_beat,
                busy,
                &config,
            ));
        }
    }
}

/// Park for at most `timeout`. `true` means shutdown was requested.
///
/// Bounded by construction: `wait_timeout` cannot outlast `timeout`, and the
/// notify in [`Watchdog::request_stop`] cuts even that short. A poisoned mutex
/// is treated as shutdown rather than a panic, because a reporter that panics
/// during a stall removes the only description of it.
fn park(state: &Arc<State>, timeout: Duration) -> bool {
    let Ok(stop) = state.stop.lock() else {
        return true;
    };
    if *stop {
        return true;
    }
    match state.wake.wait_timeout(stop, timeout) {
        Ok((stop, _)) => *stop,
        Err(_) => true,
    }
}

fn observe(
    state: &Arc<State>,
    event: WatchdogEvent,
    since_beat: Duration,
    busy: usize,
    config: &WatchdogConfig,
) -> WatchdogReport {
    let phase = Phase(state.phase.load(Ordering::Relaxed)).label();
    // The thread dump is the expensive half and only earns its cost on a stall:
    // it is what separates a futex deadlock from a busy loop from a blocked
    // syscall. Routine `alive` and `recovered` lines skip it.
    let thread_rows = if event == WatchdogEvent::Stalled {
        thread_rows(config.max_threads_dumped)
    } else {
        Vec::new()
    };
    WatchdogReport {
        event,
        phase,
        since_beat,
        busy,
        rss_kib: rss_kib(),
        threads: thread_count(),
        open_fds: open_fds(),
        thread_rows,
    }
}

#[cfg(target_os = "linux")]
fn proc_status_field(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

#[cfg(target_os = "linux")]
fn rss_kib() -> Option<u64> {
    proc_status_field("VmRSS:")
}

#[cfg(target_os = "linux")]
fn thread_count() -> Option<usize> {
    proc_status_field("Threads:").and_then(|value| usize::try_from(value).ok())
}

#[cfg(target_os = "linux")]
fn open_fds() -> Option<usize> {
    Some(std::fs::read_dir("/proc/self/fd").ok()?.count())
}

/// `tid:name:state:wchan` for each live thread, capped at `limit`.
///
/// `state` and `wchan` together are what make a stall diagnosable: `S` parked on
/// `futex_wait_queue` is a lock cycle, `R` with an empty `wchan` is a busy loop,
/// and `D` is a blocked syscall. None of those look different from outside the
/// process.
#[cfg(target_os = "linux")]
fn thread_rows(limit: usize) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for entry in entries.flatten() {
        if rows.len() >= limit {
            break;
        }
        let tid = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.path();
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .map(|text| text.trim().replace([',', ':'], "_"))
                .unwrap_or_else(|_| "?".to_owned())
        };
        let state = std::fs::read_to_string(dir.join("stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
            })
            .unwrap_or_else(|| "?".to_owned());
        rows.push(format!("{tid}:{}:{state}:{}", read("comm"), read("wchan")));
    }
    rows
}

#[cfg(not(target_os = "linux"))]
fn rss_kib() -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
fn thread_count() -> Option<usize> {
    None
}

#[cfg(not(target_os = "linux"))]
fn open_fds() -> Option<usize> {
    None
}

#[cfg(not(target_os = "linux"))]
fn thread_rows(_limit: usize) -> Vec<String> {
    Vec::new()
}
