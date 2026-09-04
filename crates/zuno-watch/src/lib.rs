//! Filesystem watching that publishes coalesced change events on a bounded channel.
//!
//! Port of `packages/core/src/filesystem/watcher.ts`, replacing its
//! `@parcel/watcher` native binding with [`notify`], which is pure Rust and links
//! into the binary. There is consequently no `hasNativeBinding()` degradation
//! path (`watcher.ts:52`, `watcher.ts:74`) and no per-platform `.node` addon to
//! ship.
//!
//! # The overflow policy, stated once
//!
//! Every event a consumer receives arrives over a **bounded**
//! [`tokio::sync::mpsc`] channel of [`WatchOptions::capacity`] slots. Nothing in
//! this crate ever grows a queue in response to load. Under pressure it degrades
//! in this order:
//!
//! 1. **Coalesce.** Notifications for one path collapse to one event
//!    ([`debounce`]). A save that produces three inotify notifications produces
//!    one event; a file touched a hundred times in one window produces one event.
//!    Nothing is lost — this is the ordinary case, not a failure mode.
//! 2. **Hold.** If the channel is full, the batch goes back into the coalescing
//!    buffer and is retried one quiet period later. A stalled consumer therefore
//!    causes *more* coalescing, not queue growth, and the watcher thread never
//!    blocks on it.
//! 3. **Drop, visibly.** If the buffer itself reaches
//!    [`WatchOptions::max_pending`] distinct paths, further **new** paths are
//!    discarded and counted. The count is delivered as
//!    [`WatchEvent::Overflow`] — ahead of the surviving batch, so a consumer
//!    learns its view has a hole before it acts on a partial one. Paths already
//!    held keep merging, because merging costs no memory.
//!
//! A drop in step 1 is information-preserving; a drop in step 3 is not, and the
//! difference is exactly why one is silent and the other is an event. A consumer
//! that receives [`WatchEvent::Overflow`] must rescan.
//!
//! Loss the platform inflicts is reported through the same event, because a hole is
//! a hole whoever made it: a backend rescan notice (inotify's `IN_Q_OVERFLOW`,
//! FSEvents' `MustScanSubDirs`) and a watch limit that left a subtree unwatched
//! (inotify's `MaxFilesWatch`) both become a [`WatchEvent::Overflow`]. Neither
//! states how much was lost, so its count is a floor.
//!
//! A loss that does not heal is re-reported on a floor of
//! [`LOST_COVERAGE_REPORT_INTERVAL`] rather than on every occurrence, because
//! `notify` re-delivers such a failure without bound and the consumer's response to
//! it is a full rescan. That floor is keyed **per loss class**, not per watch: an
//! exhausted watch limit and a failing read on the notification fd hold separate
//! budgets, so a frequent cause can never spend the report belonging to a rare one.
//! Each admitted report keeps its own `WARN`, so a watch that never recovers stays
//! visible to an operator whether or not anything reads
//! [`Watcher::lost_coverage`] — which is additionally sticky for the life of the
//! watch, but nothing here depends on that being read.
//!
//! A read error the backend simply repeats loses nothing and is therefore not a
//! loss at all: `notify` forwards every non-`WouldBlock` read error to the callback
//! and then loops round to read again, so an `EINTR` arrives here with the events
//! still queued. Reporting it would be a false hole — and a false hole that spends
//! an operator's attention on the one condition they cannot do anything about. It
//! fails closed if it stops being transient, though: a read still being repeated
//! after `RETRIED_READS_BEFORE_LOSS` consecutive attempts with not one notification
//! delivered in between means nothing is draining the bounded kernel queue, and that
//! is reported as lost coverage like any other.
//!
//! # What this crate still cannot see
//!
//! Two losses reach no consumer at all, because `notify` 8.2 does not hand them to
//! the callback in any form. Both leave a subtree dark while every counter here
//! reads healthy, so [`Watcher::lost_coverage`] returning `false` and a stream with
//! no [`WatchEvent::Overflow`] on it are **not** a completeness guarantee; a
//! consumer that must not miss a change needs a periodic rescan of its own.
//!
//! - **Windows, for every kernel-side loss.** `ReadDirectoryChangesW` never sets a
//!   rescan flag and never delivers an `Err` — `notify-8.2.0/src/windows.rs`
//!   contains no `Flag::Rescan` at all. A buffer overrun arrives as
//!   `ERROR_NOTIFY_ENUM_DIR` (1022) in the unidentified-error arm at
//!   `windows.rs:355-368`, which logs and then calls `request.unwatch()`. So the
//!   loss is not merely unreported: **the directory watch itself is removed for the
//!   life of the process**, the handler is never told, and [`Watcher::active_root`]
//!   keeps naming a scope that will never deliver anything again. There is no probe
//!   this crate could use to notice — that backend's `unwatch` returns `Ok(())`
//!   whether or not the watch still exists (`windows.rs:546-559`, and
//!   `remove_watch` at `windows.rs:243` is a silent no-op for an absent key) — and
//!   blind periodic re-registration would have to assume loss every time, which
//!   would cost every Windows consumer a permanent rescan duty cycle. So this is
//!   recorded rather than papered over.
//! - **Linux, an auto-add that fails for anything but the watch limit.** When a
//!   directory appears under a recursive watch, `notify` adds a watch for it and
//!   forwards only [`NotifyErrorKind::MaxFilesWatch`] to the handler
//!   (`inotify.rs:383-395`). `EACCES` from `inotify_add_watch` becomes `Io`
//!   (`inotify.rs:455`) and is dropped on the floor — and because the recursive add
//!   propagates it with `?` out of the `WalkDir` loop (`inotify.rs:406-412`), the
//!   sibling directories after it are abandoned too. A directory `WalkDir` cannot
//!   even read is dropped one step earlier, by `filter_dir` (`inotify.rs:523-532`),
//!   which discards its error along with it. So a directory created unreadable under
//!   the watched root is dark with no signal, and it can take its later siblings
//!   with it. The same failure at [`Watcher::start`] is *not* silent: there the
//!   error is returned as [`WatchError::Notify`] rather than delivered to the
//!   callback, and the watch does not start.
//!
//! # Why the watcher thread cannot be blocked
//!
//! [`notify`] runs its callback on a thread it owns. Blocking there stops the
//! inotify queue being drained, and the kernel's queue is itself bounded
//! (`/proc/sys/fs/inotify/max_queued_events`, 16384 by default) — overrun it and
//! the kernel sets `IN_Q_OVERFLOW` and *silently discards* events. So the
//! callback only ever takes a mutex around the coalescing buffer, and the only
//! send it performs is [`tokio::sync::mpsc::Sender::try_send`], from a different
//! thread. No path in this crate calls a blocking or awaiting send. When the queue
//! overflows anyway, the loss is silent only to the kernel: the flag the backend
//! sets on it becomes [`WatchEvent::Overflow`].
//!
//! # Threads, not tasks
//!
//! The flush loop is a plain [`std::thread`]. `try_send` needs no reactor, so the
//! crate can be constructed and can publish outside a Tokio runtime, and the
//! loop's timing does not depend on runtime scheduling — which is what lets its
//! tests assert on real deadlines instead of hoping a `sleep` was long enough.

pub mod debounce;
pub mod flags;
pub mod ignore;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::event::{ModifyKind, RenameMode};
use notify::{
    ErrorKind as NotifyErrorKind, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _,
};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};
use zuno_paths::Env;

pub use crate::debounce::{ChangeKind, Debouncer, FileEvent, Flush};
pub use crate::flags::{Decision, DisabledReason};
pub use crate::ignore::{Filter, FilterBuilder, PatternError};

/// Default trailing debounce.
///
/// Long enough that a multi-write save (`Create` + `Modify` + metadata) lands in
/// one window on any filesystem, short enough that a human editing a file sees
/// the effect as immediate. `@parcel/watcher` batches on its own schedule which
/// the oracle never configures, so there is no number to port here.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(100);

/// Default ceiling on how long the oldest pending change is held.
///
/// A build or a `git checkout` touches something every few milliseconds for
/// seconds at a time, so the trailing debounce alone would never elapse and the
/// consumer would starve for the length of the build. One second bounds staleness
/// without giving up most of the coalescing.
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(1);

/// Default publish channel capacity, in events.
///
/// Sized to absorb one full flush of a branch switch without dropping: a large
/// `git checkout` touching ~1,000 distinct files is the shape this has to survive,
/// and 1,024 is the next power of two above it. The cost is bounded and small —
/// 1,024 slots of [`WatchEvent`] is on the order of 40 KiB of queue plus the path
/// strings actually queued. Raising it does not make the watcher more correct; it
/// only trades staleness for memory, and past this size a consumer that cannot
/// keep up wants [`WatchEvent::Overflow`] and a rescan, not a longer queue.
pub const DEFAULT_CAPACITY: usize = 1_024;

/// Default ceiling on distinct paths held in the coalescing buffer.
///
/// Four times [`DEFAULT_CAPACITY`], so a consumer that has stalled completely
/// still coalesces four flush-windows' worth of distinct paths before anything is
/// discarded. Beyond that the honest answer is [`WatchEvent::Overflow`]: a buffer
/// large enough to hold every path in a monorepo is just an unbounded queue with
/// extra steps.
pub const DEFAULT_MAX_PENDING: usize = 4_096;

/// How long the flush loop sleeps when there is nothing pending.
///
/// A wake from the notify callback is what normally ends this sleep; the timeout
/// only bounds how long shutdown can take if a wake is missed.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// What one platform-reported loss of unstated size adds to the drop count.
///
/// Neither inotify's `IN_Q_OVERFLOW` nor FSEvents' `MustScanSubDirs` says how many
/// notifications were discarded, and the only correct consumer response is the same
/// whatever the number is: rescan. So one is recorded rather than a guess or a
/// sentinel, which keeps [`WatchEvent::Overflow`]'s count a floor on the loss
/// instead of an invention.
const UNKNOWN_LOSS: u64 = 1;

/// How often a platform loss that does not heal may reach the consumer again.
///
/// `notify` re-delivers a persistent failure without bound. Its inotify drain loop
/// hands the callback `Err` and loops again on any read error that is not
/// `WouldBlock` — there is no `break` on that arm
/// (`notify-8.2.0/src/inotify.rs:367-374`) — and it re-attempts a failed recursive
/// add for every batch of newly created directories (`inotify.rs:383-395`), a
/// condition that only an operator can clear. Reporting every occurrence would make
/// [`WatchEvent::Overflow`], whose consumer response is a full rescan, a duty cycle
/// driven by a broken watch, and would write a log line per iteration on the one
/// thread that must never stall. Reporting only the first would instead leave a
/// consumer stale for as long as the process runs, because the hole does not close
/// on its own.
///
/// A floor is therefore the answer rather than a latch: at one minute a consumer is
/// at most a minute behind a watch it cannot fix, and the report costs roughly four
/// orders of magnitude less than the loop producing the notifications. It
/// deliberately does not back off — a hole that persists has to stay visible rather
/// than fade out — and it deliberately does not apply to a backend rescan notice,
/// which is a distinct loss each time it is emitted (see `ingest_at`).
///
/// Every loss class holds its own floor, because a floor keyed per watch lets a
/// frequent benign cause spend the budget of the rare consequential one: one
/// interrupted read would otherwise consume the report and leave the next fifty
/// watch-limit failures with nothing to say. The interval is a crate constant and
/// not a [`WatchOptions`] field on purpose. It bounds a cost — one log line and one
/// consumer rescan per class — that a caller has no input on, nothing derives it
/// from an event, and the only thing lowering it could buy is the flood it exists to
/// prevent. What a host can still bound is the *response*: the consumer decides what
/// [`WatchEvent::Overflow`] costs it.
pub const LOST_COVERAGE_REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// How many consecutive repeated reads mean the queue is not being drained at all.
///
/// A read the backend repeats loses nothing, so one of them is not a hole (see
/// [`read_will_be_retried`]). A read *still* being repeated after this many
/// consecutive attempts, with not one notification delivered in between, is a
/// different condition: nothing is draining the kernel queue, that queue is bounded,
/// and what it discards once it fills is gone. So the retryable class fails closed
/// into a reported loss rather than staying quiet for the life of the process.
///
/// Audited in both directions. Downward, if the threshold is reached when nothing
/// was actually lost, the cost is one admitted report — one consumer rescan and one
/// log line — floored per class like every other report, and a single delivered
/// notification resets the count, so no isolated interrupted read can walk up to it.
/// Upward, a genuinely stuck drain loop is reported after 1024 iterations of a loop
/// that spins as fast as `read` can return, which is immediate in wall-clock terms
/// rather than a delay an operator would notice. Nothing derives this number from an
/// event, a peer, or any other input: it counts this crate's own observations, and
/// no caller can raise it.
const RETRIED_READS_BEFORE_LOSS: u64 = 1_024;

/// Something the consumer needs to know about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    /// A path changed. The oracle's `file.watcher.updated`
    /// (`schema/src/filesystem-watcher.ts:6-13`).
    Changed(FileEvent),
    /// Changes were discarded without being reported.
    ///
    /// Delivered before the batch it precedes. `dropped` counts paths lost since
    /// the previous `Overflow`, not since the start, so a consumer can act on the
    /// number without keeping a running total. It is a *floor*: a loss the platform
    /// reports without a magnitude — a kernel queue overflow, or a watch limit that
    /// left a subtree unwatched — counts as one, because the required response is a
    /// rescan either way.
    Overflow {
        /// How many paths were discarded unreported.
        dropped: u64,
    },
}

/// Why a watch could not be established.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// An ignore pattern would not compile.
    #[error(transparent)]
    Pattern(#[from] PatternError),
    /// `notify` could not create the platform watcher or add the watch.
    ///
    /// The oracle logs and continues with an inert service here
    /// (`watcher.ts:96-101`). This surfaces the error instead so the caller
    /// decides; [`Watcher::start`] never reaches it for the disabled case, which
    /// is not an error.
    #[error("failed to watch {path}: {source}")]
    Notify {
        /// The path that could not be watched.
        path: PathBuf,
        /// What `notify` said.
        #[source]
        source: notify::Error,
    },
}

/// How to watch.
#[derive(Clone, Debug)]
pub struct WatchOptions {
    root: PathBuf,
    vcs_dir: Option<PathBuf>,
    env: Env,
    watch_missing_ancestors: bool,
    extra_ignore: Vec<String>,
    whitelist: Vec<String>,
    gitignore: bool,
    require_git: bool,
    debounce: Duration,
    max_wait: Duration,
    capacity: usize,
    max_pending: usize,
}

impl WatchOptions {
    /// Watch `root` with production defaults and the process environment.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            vcs_dir: None,
            env: Env::from_process(),
            watch_missing_ancestors: false,
            extra_ignore: Vec::new(),
            whitelist: Vec::new(),
            gitignore: false,
            require_git: true,
            debounce: DEFAULT_DEBOUNCE,
            max_wait: DEFAULT_MAX_WAIT,
            capacity: DEFAULT_CAPACITY,
            max_pending: DEFAULT_MAX_PENDING,
        }
    }

    /// Use an explicit environment instead of the process's.
    ///
    /// The only way a test can vary the two experimental flags: this workspace
    /// forbids `unsafe`, and `std::env::set_var` is `unsafe` in edition 2024, so
    /// no test may mutate the real environment. Same rationale as
    /// [`zuno_paths::Env`] itself.
    #[must_use]
    pub fn env(mut self, env: Env) -> Self {
        self.env = env;
        self
    }

    /// Keep a requested directory live even when it does not exist yet.
    ///
    /// The watcher initially subscribes to the nearest existing ancestor
    /// non-recursively. [`Watcher::reconcile`] moves that subscription closer as
    /// missing path components appear, and switches to recursive mode only once
    /// the requested directory itself exists. This preserves create-after-start
    /// discovery without recursively watching an unrelated home or filesystem
    /// tree.
    #[must_use]
    pub fn watch_missing_ancestors(mut self) -> Self {
        self.watch_missing_ancestors = true;
        self
    }

    /// Also watch the VCS metadata directory — `.git` (`watcher.ts:112-124`).
    ///
    /// Only `HEAD` inside it is reported; see [`is_vcs_reportable`].
    #[must_use]
    pub fn vcs_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.vcs_dir = Some(dir.into());
        self
    }

    /// Add `watcher.ignore` patterns (`v1/config/config.ts:51`).
    #[must_use]
    pub fn extra_ignore<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra_ignore
            .extend(patterns.into_iter().map(Into::into));
        self
    }

    /// Add the `watcher.ignore` patterns held by a config document.
    #[must_use]
    pub fn watcher_config(self, config: Option<&zuno_config::schema::WatcherConfig>) -> Self {
        match config.and_then(|config| config.ignore.as_deref()) {
            Some(patterns) => self.extra_ignore(patterns.iter().cloned()),
            None => self,
        }
    }

    /// Patterns that force a path to be reported (`filesystem/ignore.ts:51-53`).
    #[must_use]
    pub fn whitelist<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.whitelist.extend(patterns.into_iter().map(Into::into));
        self
    }

    /// Consult `.gitignore` files as well as the built-in pattern list.
    ///
    /// Off by default because the oracle has no gitignore layer; see
    /// [`crate::ignore`].
    #[must_use]
    pub fn gitignore(mut self, enabled: bool) -> Self {
        self.gitignore = enabled;
        self
    }

    /// Whether `.gitignore` needs a `.git` above it to apply. Default `true`.
    #[must_use]
    pub fn require_git(mut self, required: bool) -> Self {
        self.require_git = required;
        self
    }

    /// Override the trailing debounce. Tests shorten it to stay fast.
    #[must_use]
    pub fn debounce(mut self, quiet: Duration) -> Self {
        self.debounce = quiet;
        self
    }

    /// Override the ceiling on how long the oldest pending change is held.
    #[must_use]
    pub fn max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    /// Override the publish channel capacity.
    ///
    /// Lowering it is how a test drives the backpressure path deterministically
    /// rather than by generating enough load to hit
    /// [`DEFAULT_CAPACITY`] by luck.
    #[must_use]
    pub fn capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.max(1);
        self
    }

    /// Override the ceiling on distinct pending paths.
    #[must_use]
    pub fn max_pending(mut self, max_pending: usize) -> Self {
        self.max_pending = max_pending.max(1);
        self
    }
}

/// Counters a consumer or a test can read without draining the channel.
#[derive(Debug, Default)]
struct Counters {
    accepted: AtomicU64,
    published: AtomicU64,
    dropped: AtomicU64,
}

/// Whether one occurrence of a repeating platform loss may be reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LossReport {
    /// The first for this watch. Worth an operator's attention.
    First,
    /// A later one, admitted because the report floor has elapsed.
    Again,
}

/// What kind of coverage a `notify` failure took away.
///
/// The classes exist to be *budgeted separately*. They differ in how often they can
/// arrive, in how consequential they are, and in what an operator can do about them,
/// so a report floor shared between them would let the cheap frequent one silence
/// the expensive rare one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LossClass {
    /// `fs.inotify.max_user_watches` is exhausted, so a subtree is unwatched for the
    /// life of the watch. Rare, and the only one an operator can fix.
    WatchLimit,
    /// The backend could not read the notification queue, so whatever that read
    /// would have returned is gone. Repeats as fast as the drain loop can spin.
    Notifications,
}

/// How a `notify::Error` handed to the callback must be treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchFailure {
    /// Coverage is gone until something outside this process changes.
    Lost(LossClass),
    /// The read did not happen, so the queue still holds its events and the backend
    /// will deliver them on the next iteration. Nothing was lost.
    Retryable,
    /// An answer to a call this crate made about one path, which
    /// [`Watcher::reconcile`] already handles.
    Incidental,
}

/// The floor for one repeating condition.
///
/// Takes the current instant as an argument rather than reading the clock, for the
/// reason [`crate::debounce`] does: both directions of the window — suppressed
/// inside it, admitted once past it — are then assertable on an explicit timeline
/// instead of by sleeping and hoping.
#[derive(Debug, Default)]
struct ReportFloor {
    /// When a report was last admitted. `None` until the first one, which is also
    /// what makes the answer to [`ReportFloor::reported`] sticky.
    admitted: Mutex<Option<Instant>>,
}

impl ReportFloor {
    /// Whether this occurrence may be reported, and whether it is the first ever.
    ///
    /// A poisoned mutex is recovered from rather than propagated, for the same
    /// reason [`Shared::lock`] recovers: a panic elsewhere must not silence the one
    /// signal that says the watch has a hole.
    fn admit(&self, now: Instant) -> Option<LossReport> {
        let mut admitted = self
            .admitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *admitted {
            Some(at) if now.saturating_duration_since(at) < LOST_COVERAGE_REPORT_INTERVAL => None,
            ever => {
                *admitted = Some(now);
                Some(if ever.is_some() {
                    LossReport::Again
                } else {
                    LossReport::First
                })
            }
        }
    }

    /// Whether this condition has ever been reported for this watch.
    fn reported(&self) -> bool {
        self.admitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
}

/// One [`ReportFloor`] per condition, so no two conditions share a budget.
#[derive(Debug, Default)]
struct LossReports {
    /// [`LossClass::WatchLimit`].
    watch_limit: ReportFloor,
    /// [`LossClass::Notifications`].
    notifications: ReportFloor,
    /// Not a loss and never reported to the consumer: this only bounds how often a
    /// retried read error is written to the log from the thread that must keep
    /// draining the kernel queue. Kept separate from both loss classes precisely so
    /// that a signal arriving at an unlucky moment cannot spend either one.
    retried: ReportFloor,
    /// Repeated reads since the last notification actually delivered. Zeroed by any
    /// delivery, which is what makes [`RETRIED_READS_BEFORE_LOSS`] a statement about
    /// a drain loop that is stuck rather than one that was merely interrupted.
    retried_reads: AtomicU64,
}

impl LossReports {
    /// The floor that budgets `class`.
    fn floor(&self, class: LossClass) -> &ReportFloor {
        match class {
            LossClass::WatchLimit => &self.watch_limit,
            LossClass::Notifications => &self.notifications,
        }
    }

    /// Whether this occurrence of `class` may be reported, and whether it is the
    /// first of its class ever.
    fn admit(&self, class: LossClass, now: Instant) -> Option<LossReport> {
        self.floor(class).admit(now)
    }

    /// Whether a *coverage loss* has ever been reported for this watch.
    ///
    /// Deliberately excludes [`LossReports::retried`]: a read the backend repeats
    /// lost nothing, so letting it latch this would turn one interrupted syscall
    /// into a watch permanently described as degraded.
    fn reported(&self) -> bool {
        self.watch_limit.reported() || self.notifications.reported()
    }
}

/// What the callback thread and the flush thread share.
struct Shared {
    debouncer: Mutex<Debouncer>,
    wake: Condvar,
    shutdown: AtomicBool,
    sender: Sender<WatchEvent>,
    counters: Counters,
    /// One report floor per loss class. Written by the callback thread and read by
    /// [`Watcher::lost_coverage`] on the consumer's thread.
    losses: LossReports,
}

impl Shared {
    /// Lock the buffer, recovering from a poisoned mutex.
    ///
    /// A panic inside the lock must not take the watch down: the alternative to
    /// recovering is that one bad event permanently stops the consumer being told
    /// about anything, which is strictly worse than continuing with whatever the
    /// buffer holds. Nothing in the guarded region can leave the buffer in a state
    /// that is unsound to observe — it is a map and three counters.
    fn lock(&self) -> MutexGuard<'_, Debouncer> {
        self.debouncer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A live watch.
///
/// Dropping it stops watching and joins the flush loop, so a caller keeps it for
/// as long as it wants events. The `notify` handle is owned here for the same
/// reason: dropping it removes the inotify watches.
pub struct Watcher {
    decision: Decision,
    shared: Arc<Shared>,
    filter: Arc<Filter>,
    requested_root: PathBuf,
    active_scope: Option<WatchScope>,
    watch_missing_ancestors: bool,
    inner: Option<RecommendedWatcher>,
    flush: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WatchScope {
    path: PathBuf,
    recursive: bool,
}

/// The receiving end of the bounded publish channel.
///
/// Single-consumer by construction. Backpressure is not a fault here — a slow
/// consumer causes coalescing, and only a consumer that stops entirely for long
/// enough will see [`WatchEvent::Overflow`].
pub struct EventStream {
    inner: Receiver<WatchEvent>,
    capacity: usize,
}

impl EventStream {
    /// Await the next event. `None` once the watcher has been dropped.
    pub async fn recv(&mut self) -> Option<WatchEvent> {
        self.inner.recv().await
    }

    /// Take the next event if one is already queued.
    #[must_use]
    pub fn try_recv(&mut self) -> Option<WatchEvent> {
        self.inner.try_recv().ok()
    }

    /// Block the current thread until the next event. For non-async consumers.
    #[must_use]
    pub fn blocking_recv(&mut self) -> Option<WatchEvent> {
        self.inner.blocking_recv()
    }

    /// The channel's fixed capacity, for a consumer that wants to assert on it.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many events are queued right now.
    ///
    /// Never exceeds [`EventStream::capacity`]; that invariant is the bounded
    /// channel, and a test asserting it is asserting the whole overflow policy.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.capacity.saturating_sub(self.inner.capacity())
    }
}

impl Watcher {
    /// Start watching, or return an inert watcher if the flags say not to.
    ///
    /// A disabled watcher is not an error and not a separate type: it yields an
    /// [`EventStream`] that never produces anything, exactly as the oracle returns
    /// an empty service (`watcher.ts:59`, `watcher.ts:130-136`). Callers therefore
    /// have no branch to write; consult [`Watcher::decision`] only if the reason
    /// is worth logging.
    ///
    /// # Errors
    ///
    /// [`WatchError::Pattern`] if an ignore pattern will not compile, or
    /// [`WatchError::Notify`] if the platform watcher cannot be created or the
    /// root cannot be watched.
    pub fn start(options: WatchOptions) -> Result<(Self, EventStream), WatchError> {
        let decision = flags::decide(&options.env);
        let filter = Arc::new(
            FilterBuilder::new(filter_root(&options))
                .extra_patterns(options.extra_ignore.iter().cloned())
                .whitelist(options.whitelist.iter().cloned())
                .gitignore(options.gitignore)
                .require_git(options.require_git)
                .build()?,
        );
        let (sender, receiver) = mpsc::channel(options.capacity);
        let stream = EventStream {
            inner: receiver,
            capacity: options.capacity,
        };
        let shared = Arc::new(Shared {
            debouncer: Mutex::new(Debouncer::new(
                options.debounce,
                options.max_wait,
                options.max_pending,
            )),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
            sender,
            counters: Counters::default(),
            losses: LossReports::default(),
        });

        if decision.is_disabled() {
            if let Decision::Disabled(reason) = &decision {
                tracing::debug!(?reason, root = %options.root.display(), "file watching disabled");
            }
            return Ok((
                Self {
                    decision,
                    shared,
                    filter,
                    requested_root: options.root,
                    active_scope: None,
                    watch_missing_ancestors: options.watch_missing_ancestors,
                    inner: None,
                    flush: None,
                },
                stream,
            ));
        }

        let (inner, active_scope) = Self::spawn_notify(&options, &shared, &filter, &decision)?;
        let flush = std::thread::Builder::new()
            .name("zuno-watch-flush".to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                move || flush_loop(&shared)
            })
            .map_err(|error| WatchError::Notify {
                path: options.root.clone(),
                source: notify::Error::io(error),
            })?;

        Ok((
            Self {
                decision,
                shared,
                filter,
                requested_root: options.root,
                active_scope,
                watch_missing_ancestors: options.watch_missing_ancestors,
                inner: Some(inner),
                flush: Some(flush),
            },
            stream,
        ))
    }

    /// Create the platform watcher and add every subscription the flags allow.
    fn spawn_notify(
        options: &WatchOptions,
        shared: &Arc<Shared>,
        filter: &Arc<Filter>,
        decision: &Decision,
    ) -> Result<(RecommendedWatcher, Option<WatchScope>), WatchError> {
        let vcs_dir = options.vcs_dir.clone();
        let mut watcher = notify::recommended_watcher({
            let shared = Arc::clone(shared);
            let filter = Arc::clone(filter);
            move |event: notify::Result<notify::Event>| {
                ingest(&shared, &filter, vcs_dir.as_deref(), event);
            }
        })
        .map_err(|source| WatchError::Notify {
            path: options.root.clone(),
            source,
        })?;

        let active_scope = project_watch_scope(options, decision)?;
        if let Some(scope) = active_scope.as_ref() {
            watcher
                .watch(&scope.path, recursive_mode(scope.recursive))
                .map_err(|source| WatchError::Notify {
                    path: scope.path.clone(),
                    source,
                })?;
        }
        // The oracle gates the `.git` subscription on nothing but the repository
        // being git (`watcher.ts:112`), so it survives the enable flag being off.
        if decision.watches_vcs()
            && let Some(dir) = options.vcs_dir.as_deref()
        {
            watcher
                .watch(dir, RecursiveMode::NonRecursive)
                .map_err(|source| WatchError::Notify {
                    path: dir.to_path_buf(),
                    source,
                })?;
        }
        Ok((watcher, active_scope))
    }

    /// Reconcile an adaptive missing-directory subscription with the filesystem.
    ///
    /// Call this after receiving an event. It is intentionally not executed from
    /// `notify`'s callback thread: some backends synchronously communicate with
    /// their event loop when a watch is added, so reconfiguration from inside the
    /// callback can deadlock. The new scope is installed before the old one is
    /// removed, keeping the transition loss-resistant.
    ///
    /// Returns `true` when the active path or recursion mode changed.
    pub fn reconcile(&mut self) -> Result<bool, WatchError> {
        if !self.watch_missing_ancestors || !self.decision.watches_project() {
            return Ok(false);
        }
        let desired =
            adaptive_watch_scope(&self.requested_root).ok_or_else(|| WatchError::Notify {
                path: self.requested_root.clone(),
                source: notify::Error::path_not_found(),
            })?;
        if self.active_scope.as_ref() == Some(&desired) {
            return Ok(false);
        }
        let Some(inner) = self.inner.as_mut() else {
            return Ok(false);
        };

        inner
            .watch(&desired.path, recursive_mode(desired.recursive))
            .map_err(|source| WatchError::Notify {
                path: desired.path.clone(),
                source,
            })?;

        if let Some(previous) = self.active_scope.as_ref()
            && previous.path != desired.path
            && let Err(error) = inner.unwatch(&previous.path)
            && !watch_is_already_gone(&error)
        {
            tracing::debug!(
                path = %previous.path.display(),
                %error,
                "failed to remove superseded filesystem watch"
            );
        }
        self.active_scope = Some(desired);
        Ok(true)
    }

    /// The logical directory requested by the caller.
    #[must_use]
    pub fn requested_root(&self) -> &Path {
        &self.requested_root
    }

    /// The directory currently registered with the platform watcher.
    #[must_use]
    pub fn active_root(&self) -> Option<&Path> {
        self.active_scope.as_ref().map(|scope| scope.path.as_path())
    }

    /// Whether the current project subscription is recursive.
    #[must_use]
    pub fn watches_recursively(&self) -> bool {
        self.active_scope
            .as_ref()
            .is_some_and(|scope| scope.recursive)
    }

    /// What the flags decided.
    #[must_use]
    pub fn decision(&self) -> &Decision {
        &self.decision
    }

    /// The filter this watch judges paths with.
    ///
    /// Exposed so a consumer that sees a `.gitignore` change can call
    /// [`Filter::invalidate`] rather than restart the watch.
    #[must_use]
    pub fn filter(&self) -> &Arc<Filter> {
        &self.filter
    }

    /// Raw notifications folded into the coalescing buffer so far.
    ///
    /// The denominator of the coalescing ratio; [`Watcher::published`] is the
    /// numerator.
    #[must_use]
    pub fn accepted(&self) -> u64 {
        self.shared.counters.accepted.load(Ordering::Relaxed)
    }

    /// Events handed to the channel so far.
    #[must_use]
    pub fn published(&self) -> u64 {
        self.shared.counters.published.load(Ordering::Relaxed)
    }

    /// Changes discarded without being reported so far.
    ///
    /// Mirrors the totals delivered as [`WatchEvent::Overflow`]; non-zero means a
    /// consumer's view of the tree has a hole. Deliberately not a path count: the
    /// pending ceiling and a refused send count paths, while a platform loss of
    /// unstated size contributes one whatever it swallowed, so this is a floor on
    /// what was missed.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.counters.dropped.load(Ordering::Relaxed)
    }

    /// Whether the platform has told this watch that coverage was lost.
    ///
    /// Sticky: it never returns to `false`, because nothing this crate can observe
    /// says an exhausted watch limit or a failing read on the notification fd was
    /// fixed. That is how a degraded watch stays *reported* instead of being
    /// announced once and forgotten — a consumer or a diagnostic can read it long
    /// after the [`WatchEvent::Overflow`] that carried the news was consumed.
    ///
    /// `false` is not a promise of completeness. It is `false` on Windows even when
    /// the watch has been silently removed, and `false` on Linux when a directory
    /// appeared that `notify` could not add a watch for; see the module docs for both.
    /// It is also `false` after a read the backend retried, because that read lost
    /// nothing.
    ///
    /// Nothing in the workspace reads this yet. The operator-visible signal for a
    /// degraded watch is deliberately not conditional on anyone doing so: it is the
    /// `WARN` that `report_lost_coverage` repeats on its floor for as long as the
    /// condition lasts. This exists so that a consumer which wants the state in its
    /// own diagnostics — `SkillCatalogService`'s watcher warnings being the intended
    /// one — can read it instead of having to remember a
    /// [`WatchEvent::Overflow`] it may have consumed before it started caring.
    #[must_use]
    pub fn lost_coverage(&self) -> bool {
        self.shared.losses.reported()
    }

    /// Distinct paths held in the coalescing buffer right now.
    ///
    /// Never exceeds [`WatchOptions::max_pending`]. That is the invariant that
    /// makes "never grow" checkable rather than asserted.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.shared.lock().pending_len()
    }
}

fn project_watch_scope(
    options: &WatchOptions,
    decision: &Decision,
) -> Result<Option<WatchScope>, WatchError> {
    if !decision.watches_project() {
        return Ok(None);
    }
    if options.watch_missing_ancestors {
        return adaptive_watch_scope(&options.root)
            .map(Some)
            .ok_or_else(|| WatchError::Notify {
                path: options.root.clone(),
                source: notify::Error::path_not_found(),
            });
    }
    Ok(Some(WatchScope {
        path: options.root.clone(),
        recursive: true,
    }))
}

/// The directory spelling every ignore judgement for this watch is anchored to.
///
/// [`Filter`] decides by stripping its root as a path prefix, so it is only correct when it is
/// anchored to the same spelling the subscription registers — that spelling is the prefix every
/// event path the platform backend reports actually carries. An adaptive subscription registers a
/// *normalized* directory ([`adaptive_watch_scope`]), so the filter must be normalized with it. A
/// fixed subscription registers the requested directory verbatim, so there the requested spelling
/// is the subscription spelling and normalizing would create the very mismatch in reverse.
fn filter_root(options: &WatchOptions) -> PathBuf {
    if options.watch_missing_ancestors {
        normalized_root(&options.root)
    } else {
        options.root.clone()
    }
}

/// A requested directory in the form the adaptive subscription will register it.
///
/// Two distinct ways the requested spelling and the registered spelling diverge, and both make
/// every ignore rule silently stop applying so that ignored paths are published as events:
///
/// - On any platform, a root reached through a symbolic link resolves to a different absolute
///   path, and the resolved one is what the backend reports.
/// - On Windows, [`std::fs::canonicalize`] returns a `\\?\` verbatim path. `\\?\C:\p` never
///   prefix-matches `C:\p`, so a caller-supplied drive path and the registered path disagree even
///   with no link involved. Canonicalizing both sides also settles component case, which
///   [`Path::strip_prefix`] compares exactly for non-prefix components.
///
/// A directory that does not exist yet cannot be canonicalized, so the nearest existing ancestor
/// is canonicalized and the missing components are re-appended: that is the path the adaptive
/// subscription registers once the directory appears, so the filter stays anchored across
/// [`Watcher::reconcile`] without being rebuilt. When nothing on the path can be canonicalized the
/// requested spelling is returned unchanged, which is what the subscription falls back to as well.
fn normalized_root(requested: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut candidate = requested.to_path_buf();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&candidate) {
            return missing
                .iter()
                .rev()
                .fold(canonical, |normalized, component| {
                    normalized.join(component)
                });
        }
        let Some(name) = candidate.file_name().map(std::ffi::OsStr::to_os_string) else {
            return requested.to_path_buf();
        };
        missing.push(name);
        if !candidate.pop() {
            return requested.to_path_buf();
        }
    }
}

fn adaptive_watch_scope(requested: &Path) -> Option<WatchScope> {
    // Normalized first, so the scope and [`filter_root`] cannot drift apart: this is the one
    // function that decides what "the requested directory" means to the platform watcher.
    let normalized = normalized_root(requested);
    let active = nearest_existing_directory(&normalized)?;
    Some(WatchScope {
        recursive: active == normalized,
        path: active,
    })
}

fn nearest_existing_directory(requested: &Path) -> Option<PathBuf> {
    let mut candidate = requested.to_path_buf();
    loop {
        if candidate.is_dir() {
            // Never turn a missing target into a subscription on the filesystem
            // root. Even non-recursive root traffic is unrelated and too broad.
            candidate.parent()?;
            return Some(std::fs::canonicalize(&candidate).unwrap_or(candidate));
        }
        if !candidate.pop() {
            return None;
        }
    }
}

const fn recursive_mode(recursive: bool) -> RecursiveMode {
    if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    }
}

fn watch_is_already_gone(error: &notify::Error) -> bool {
    matches!(
        error.kind,
        NotifyErrorKind::PathNotFound | NotifyErrorKind::WatchNotFound
    )
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // The notify handle goes first so no new notification can arrive while
        // the flush loop is being wound down.
        drop(self.inner.take());
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if let Some(handle) = self.flush.take() {
            drop(handle.join());
        }
    }
}

/// Whether a path under the VCS metadata directory is worth reporting.
///
/// The oracle reads `.git`'s entries at subscribe time and ignores all of them
/// except `HEAD` (`watcher.ts:117-120`), so the subscription exists to notice one
/// thing: `HEAD` changing, i.e. a branch switch. Because that list is a snapshot,
/// an entry created in `.git` *after* subscribing slips through the oracle's
/// filter; this states the intent directly instead, which is a strict narrowing.
/// Recorded in the project's engineering notes.
#[must_use]
pub fn is_vcs_reportable(vcs_dir: &Path, path: &Path) -> bool {
    path == vcs_dir.join("HEAD")
}

/// Map one `notify` event into the coalescing buffer.
///
/// Runs on `notify`'s own thread, so it does the least possible work: classify,
/// filter, take a mutex, merge, wake. Two deliberate choices about that thread:
///
/// - The ignore decision is made *before* the lock, because it can read a
///   `.gitignore` ([`crate::ignore`]) and holding the coalescing buffer across that
///   would put the flush thread behind filesystem I/O for the length of a flood.
/// - `is_dir` is passed as a closure ([`Filter::is_ignored_with`]), so the `stat`
///   that answers it happens only when a gitignore rule actually reads it. With no
///   `.gitignore` chain configured — the only shape any first-party caller starts —
///   this function performs no filesystem call at all.
///
/// Not yet off this thread, and stated rather than implied: with
/// [`WatchOptions::gitignore`] enabled, the first path judged under a directory
/// still loads that directory's `.gitignore` here, so a burst that first touches
/// many directories pays one open-and-parse each on the thread that must drain the
/// kernel queue.
fn ingest(
    shared: &Arc<Shared>,
    filter: &Filter,
    vcs_dir: Option<&Path>,
    event: notify::Result<notify::Event>,
) {
    ingest_at(shared, filter, vcs_dir, event, Instant::now());
}

/// [`ingest`] on an explicit instant.
///
/// The clock is read exactly once per notification, at the edge, and everything
/// below here takes the instant as an argument — the way [`crate::debounce`] already
/// does. That is what lets a test drive the report floor's window through the real
/// production path, in both directions, without sleeping for a minute and hoping.
fn ingest_at(
    shared: &Arc<Shared>,
    filter: &Filter,
    vcs_dir: Option<&Path>,
    event: notify::Result<notify::Event>,
    now: Instant,
) {
    let event = match event {
        Ok(event) => event,
        Err(error) => {
            match classify_failure(&error) {
                WatchFailure::Lost(class) => report_lost_coverage(shared, &error, class, now),
                WatchFailure::Retryable => report_retried_read(shared, &error, now),
                // A watch error is not a change, and the oracle's callback likewise
                // ignores its `_error` parameter (`watcher.ts:83`).
                WatchFailure::Incidental => tracing::debug!(%error, "watch error"),
            }
            return;
        }
    };
    // Any delivered notification proves the drain loop is getting through, which is
    // what distinguishes a read that was interrupted from a queue nothing is
    // draining. Relaxed and unconditional: it is one store on a thread that is about
    // to take a mutex anyway, and it must count the events this crate then filters
    // out as much as the ones it keeps — the backend read them either way.
    shared.losses.retried_reads.store(0, Ordering::Relaxed);
    // The backend saying "notifications were lost" outranks whatever path it names
    // — inotify names none (`IN_Q_OVERFLOW`), FSEvents names the subtree root
    // (`MustScanSubDirs`), and in both cases the loss is wider than that path, so
    // reading it as a change to one path is how the hole went unreported.
    if event.need_rescan() {
        // Not put through the report floor that [`report_lost_coverage`] applies,
        // and that asymmetry is the point: a backend emits this notice only when a
        // loss actually happened, so each one is news, and suppressing one would
        // hide a hole the consumer has not rescanned for yet. What the asymmetry
        // costs, stated rather than implied: every notice inside one flush window
        // still leaves as a single `Overflow`, but a writer that keeps overrunning
        // the kernel queue under a watched root gets one `Overflow` per window — up
        // to ten a second at [`DEFAULT_DEBOUNCE`] — and each one is a full rescan at
        // the consumer. Coalescing that belongs where the cost of a rescan is known,
        // which is the consumer; the only alternative here is discarding a genuine
        // distinct loss.
        tracing::debug!(?event, "watch backend dropped notifications");
        record_hole(shared, now);
        return;
    }
    let mut accepted = classify(&event);
    accepted.retain(|(path, kind)| match vcs_dir {
        Some(dir) if path.starts_with(dir) => is_vcs_reportable(dir, path),
        _ => !filter.is_ignored_with(path, || *kind != ChangeKind::Unlink && path.is_dir()),
    });
    if accepted.is_empty() {
        return;
    }
    let mut guard = shared.lock();
    for (path, kind) in accepted {
        guard.accept(path, kind, now);
    }
    shared
        .counters
        .accepted
        .store(guard.accepted(), Ordering::Relaxed);
    drop(guard);
    shared.wake.notify_one();
}

/// How a watcher error handed to the callback must be treated.
///
/// [`NotifyErrorKind::MaxFilesWatch`] is the consequential one: inotify returns it
/// part-way through a recursive add when `fs.inotify.max_user_watches` is
/// exhausted and then stops adding, so every subtree after it is dark for the life
/// of the watch. An I/O failure while the backend drains the kernel queue is the
/// same shape — whatever that read would have returned is unrecoverable — *unless*
/// the read never happened. `notify`'s drain loop hands the callback every
/// non-`WouldBlock` read error and then loops round to read again with no `break`
/// (`notify-8.2.0/src/inotify.rs:367-374`), and `inotify-0.11.5` returns
/// `io::Error::last_os_error()` without retrying `EINTR`
/// (`inotify-0.11.5/src/inotify.rs:206-219`), so an interrupted read reaches this
/// function with its events still in the kernel queue and the next iteration about
/// to deliver them. Calling that lost coverage is a false hole, and — because a
/// report is a budgeted resource — a false hole that silences the one condition an
/// operator could have fixed.
///
/// The remaining kinds answer a call this crate made about one path, and
/// [`Watcher::reconcile`] already handles those; treating them as holes would make
/// every short-lived directory cost the consumer a full rescan. `notify` never hands
/// them to a callback in the first place: `grep 'handle_event(Err'` across
/// `notify-8.2.0/src` matches exactly `inotify.rs:373` and `inotify.rs:389`, so that
/// arm is defensive rather than reachable.
fn classify_failure(error: &notify::Error) -> WatchFailure {
    match &error.kind {
        NotifyErrorKind::MaxFilesWatch => WatchFailure::Lost(LossClass::WatchLimit),
        NotifyErrorKind::Io(io) if read_will_be_retried(io.kind()) => WatchFailure::Retryable,
        NotifyErrorKind::Io(_) => WatchFailure::Lost(LossClass::Notifications),
        _ => WatchFailure::Incidental,
    }
}

/// Whether an I/O error means the read transferred nothing and will be repeated.
///
/// Both kinds leave the notification queue exactly as it was: `Interrupted`
/// (`EINTR`) because a signal arrived before the read completed, and `WouldBlock`
/// (`EAGAIN`) because there was nothing to read — `notify` handles the latter itself
/// and never forwards it, so listing it here is defensive. Every other kind either
/// consumed events or means the descriptor will not produce them again: `EIO`,
/// `EINVAL` (the buffer could not hold the next event, so that event cannot be
/// read at all), `ENOMEM`, and the `UnexpectedEof` that `inotify-rs` synthesises
/// when `read` returns `0` (`inotify-0.11.5/src/inotify.rs:210-215`). Those are lost
/// coverage, and the classification fails **closed** for anything it does not
/// recognise: an unknown io kind is treated as a loss, not as retryable.
fn read_will_be_retried(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
    )
}

/// Report a loss of coverage, at most once per class per
/// [`LOST_COVERAGE_REPORT_INTERVAL`].
///
/// The floor is the whole reason this exists: every caller arrives from a `notify`
/// failure that repeats for as long as the condition lasts, so reporting each
/// occurrence turns one broken watch into an unbounded log flood on the drain thread
/// and a rescan duty cycle at the consumer. A suppressed occurrence is not the same
/// as nothing having happened — the admitted one already told the consumer to
/// rescan — and the floor is short enough that a consumer of a watch that never
/// recovers is re-told rather than left stale forever.
///
/// Two things make the floor safe in the *other* direction, where a bound reduces
/// what an operator is told. It is per class, so the classes cannot silence each
/// other; and every admitted report warns, rather than the first warning and the
/// rest being downgraded, because no client surface reads
/// [`Watcher::lost_coverage`] today and a condition that lasts for hours must not be
/// evidenced by a single line at minute zero.
fn report_lost_coverage(
    shared: &Arc<Shared>,
    error: &notify::Error,
    class: LossClass,
    now: Instant,
) {
    match shared.losses.admit(class, now) {
        // Worth an operator's attention rather than a debug line: unlike a queue
        // overflow this hole does not close on its own, so a rescan reports the same
        // missing subtree next time. The message is per class because the remedy is:
        // the watch limit is a number an operator can raise, and a failing read on
        // the notification descriptor is not.
        Some(report) => {
            let first = report == LossReport::First;
            match class {
                LossClass::WatchLimit => {
                    tracing::warn!(%error, first, "filesystem watch lost coverage");
                }
                LossClass::Notifications => {
                    tracing::warn!(%error, first, "filesystem watch stopped reading notifications");
                }
            }
            record_hole(shared, now);
        }
        None => tracing::trace!(%error, ?class, "coverage loss already reported this window"),
    }
}

/// Note a read the backend will repeat, and decide whether it is still one.
///
/// The count is the whole point. A single repeated read is not a loss and must not
/// spend a loss class's report budget, but "the backend will read again" is only true
/// while it eventually does, so the condition fails closed at
/// [`RETRIED_READS_BEFORE_LOSS`] into a reported loss instead of staying a `debug!`
/// forever.
fn report_retried_read(shared: &Arc<Shared>, error: &notify::Error, now: Instant) {
    let consecutive = shared
        .losses
        .retried_reads
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if consecutive >= RETRIED_READS_BEFORE_LOSS {
        // Not reset afterwards: the drain loop is stuck until something is delivered,
        // and the per-class floor is what keeps that from becoming a stream of
        // reports.
        report_lost_coverage(shared, error, LossClass::Notifications, now);
        return;
    }
    // Bounded like a report although it is not one: the drain loop repeats this as
    // fast as it can call `read`, on the thread that must never stall, so a line per
    // occurrence is the same flood the report floor exists to prevent. Its budget is
    // its own, never a loss class's.
    match shared.losses.retried.admit(now) {
        Some(_) => {
            tracing::debug!(%error, consecutive, "watch read did not complete; the backend retries");
        }
        None => tracing::trace!(%error, consecutive, "watch read did not complete, again"),
    }
}

/// Record a loss this crate did not cause, so the consumer is told to rescan.
///
/// Goes through the debouncer rather than the channel on purpose: the flush loop
/// owns every send, so a hole is delivered by exactly the path that already
/// delivers [`WatchEvent::Overflow`] ahead of a batch, and every hole recorded
/// inside one flush window leaves as a single overflow event rather than one per
/// notification. Across windows it is [`report_lost_coverage`]'s per-class floor,
/// not this, that keeps a repeating failure from becoming a stream of them.
fn record_hole(shared: &Arc<Shared>, now: Instant) {
    shared.lock().record_dropped(UNKNOWN_LOSS, now);
    shared.wake.notify_one();
}

/// Split one `notify` event into the (path, kind) pairs it means.
///
/// A rename reported as [`RenameMode::Both`] carries two paths in one event, in
/// `(from, to)` order, and means two different things about them — the only case
/// where one notification is more than one change.
fn classify(event: &notify::Event) -> Vec<(PathBuf, ChangeKind)> {
    match event.kind {
        // Opening and closing a file is not a change to it. `@parcel/watcher`
        // reports only create/update/delete (`watcher.ts:85-89`), so mapping
        // these to anything would invent events the oracle never publishes.
        EventKind::Access(_) => Vec::new(),
        EventKind::Create(_) => tagged(event, ChangeKind::Add),
        EventKind::Remove(_) => tagged(event, ChangeKind::Unlink),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => tagged(event, ChangeKind::Unlink),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => tagged(event, ChangeKind::Add),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            let mut paths = event.paths.iter();
            let mut pairs = Vec::with_capacity(2);
            if let Some(from) = paths.next() {
                pairs.push((from.clone(), ChangeKind::Unlink));
            }
            if let Some(to) = paths.next() {
                pairs.push((to.clone(), ChangeKind::Add));
            }
            pairs
        }
        // `Name(Any)`/`Name(Other)` give no direction, and `Any`/`Other` give no
        // kind. `Change` is the conservative reading of all of them: it tells the
        // consumer to re-read, which is correct whether the path was written or
        // merely moved into place, and a re-read of a path that has gone away
        // fails harmlessly. Guessing `Unlink` here would evict live files.
        _ => tagged(event, ChangeKind::Change),
    }
}

/// Every path in `event`, all with the same kind.
fn tagged(event: &notify::Event, kind: ChangeKind) -> Vec<(PathBuf, ChangeKind)> {
    event
        .paths
        .iter()
        .map(|path| (path.clone(), kind))
        .collect()
}

/// Wait for a flush to come due, publish it, repeat until shutdown.
fn flush_loop(shared: &Arc<Shared>) {
    while !shared.shutdown.load(Ordering::Acquire) {
        let Some(flush) = wait_for_flush(shared) else {
            continue;
        };
        if !publish(shared, flush) {
            return;
        }
    }
    // A final drain, so a change observed just before shutdown is not silently
    // discarded by the drop.
    let flush = shared.lock().flush();
    if !flush.is_empty() {
        let _consumer_may_be_gone = publish(shared, flush);
    }
}

/// Block until a flush is due, then take it.
///
/// `None` means "woken with nothing to do" — a spurious wake or a shutdown — and
/// the caller re-checks the shutdown flag.
fn wait_for_flush(shared: &Arc<Shared>) -> Option<Flush> {
    let mut guard = shared.lock();
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            return None;
        }
        let now = Instant::now();
        let wait = match guard.deadline() {
            None => IDLE_POLL,
            Some(deadline) if now >= deadline => return Some(guard.flush()),
            Some(deadline) => deadline.saturating_duration_since(now).min(IDLE_POLL),
        };
        guard = shared
            .wake
            .wait_timeout(guard, wait)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

/// Hand a flush to the channel. `false` once the consumer is gone.
///
/// The lock is *not* held here: every `try_send` happens with the buffer
/// unlocked, so the notify callback is never waiting on the channel.
fn publish(shared: &Arc<Shared>, flush: Flush) -> bool {
    let Flush { events, dropped } = flush;
    if dropped > 0 {
        shared
            .counters
            .dropped
            .fetch_add(dropped, Ordering::Relaxed);
        // Ahead of the batch: a consumer learns its view has a hole before it
        // acts on a partial one.
        match shared.sender.try_send(WatchEvent::Overflow { dropped }) {
            Ok(()) => (),
            Err(TrySendError::Closed(_)) => return false,
            Err(TrySendError::Full(_)) => {
                shared.lock().record_dropped(dropped, Instant::now());
            }
        }
    }
    let mut iterator = events.into_iter();
    let mut published = 0_u64;
    let refused = loop {
        let Some(event) = iterator.next() else {
            break None;
        };
        match shared.sender.try_send(WatchEvent::Changed(event)) {
            Ok(()) => published += 1,
            Err(TrySendError::Closed(_)) => {
                shared
                    .counters
                    .published
                    .fetch_add(published, Ordering::Relaxed);
                return false;
            }
            // Full is not a failure: the rest of the batch goes back into the
            // buffer to be coalesced with whatever arrives next, and is retried
            // one quiet period later.
            Err(TrySendError::Full(WatchEvent::Changed(event))) => break Some(event),
            Err(TrySendError::Full(_)) => break None,
        }
    };
    shared
        .counters
        .published
        .fetch_add(published, Ordering::Relaxed);
    if let Some(event) = refused {
        let now = Instant::now();
        let mut guard = shared.lock();
        guard.requeue(std::iter::once(event).chain(iterator), now);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, Flag, ModifyKind};

    fn event(kind: EventKind, paths: &[&str]) -> notify::Event {
        notify::Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: notify::event::EventAttributes::new(),
        }
    }

    /// The callback thread's collaborators, with the flush loop left to the test.
    ///
    /// [`ingest`] is what `notify` calls and [`publish`] is what the flush thread
    /// calls, so driving both by hand exercises the real hole-reporting path
    /// end to end without waiting on a real kernel queue to overflow.
    fn harness() -> (Arc<Shared>, Filter, Receiver<WatchEvent>) {
        let (sender, receiver) = mpsc::channel(8);
        let shared = Arc::new(Shared {
            debouncer: Mutex::new(Debouncer::new(
                Duration::from_millis(1),
                Duration::from_millis(1),
                8,
            )),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
            sender,
            counters: Counters::default(),
            losses: LossReports::default(),
        });
        let filter = FilterBuilder::new("/r").build().expect("built-in patterns");
        (shared, filter, receiver)
    }

    /// Everything the flush loop would deliver for what has been ingested so far.
    fn delivered(shared: &Arc<Shared>, receiver: &mut Receiver<WatchEvent>) -> Vec<WatchEvent> {
        let flush = shared.lock().flush();
        assert!(publish(shared, flush), "the consumer is still connected");
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn a_backend_rescan_notice_is_reported_as_a_hole() {
        let (shared, filter, mut receiver) = harness();
        // The inotify shape: `IN_Q_OVERFLOW` arrives as a flagged event with no
        // paths at all, so anything keyed off `event.paths` sees nothing to do.
        ingest(
            &shared,
            &filter,
            None,
            Ok(notify::Event::new(EventKind::Other).set_flag(Flag::Rescan)),
        );
        assert_eq!(
            delivered(&shared, &mut receiver),
            vec![WatchEvent::Overflow { dropped: 1 }],
            "a kernel-side loss must reach the consumer as an overflow"
        );
    }

    #[test]
    fn a_rescan_notice_carrying_a_path_is_still_a_hole() {
        let (shared, filter, mut receiver) = harness();
        // The FSEvents shape: `MustScanSubDirs` names the subtree root, and reading
        // it as a change to that one directory understates a loss that is wider.
        ingest(
            &shared,
            &filter,
            None,
            Ok(event(EventKind::Other, &["/r/sub"]).set_flag(Flag::Rescan)),
        );
        assert_eq!(
            delivered(&shared, &mut receiver),
            vec![WatchEvent::Overflow { dropped: 1 }]
        );
    }

    #[test]
    fn exhausting_the_watch_limit_is_reported_as_a_hole() {
        let (shared, filter, mut receiver) = harness();
        ingest(
            &shared,
            &filter,
            None,
            Err(notify::Error::new(NotifyErrorKind::MaxFilesWatch)
                .add_path(PathBuf::from("/r/deep"))),
        );
        assert_eq!(
            delivered(&shared, &mut receiver),
            vec![WatchEvent::Overflow { dropped: 1 }],
            "an unwatched subtree is a hole, not a debug line"
        );
    }

    /// The exact shape `notify`'s inotify drain loop repeats without bound: a read
    /// error that is not `WouldBlock` is handed to the callback and the loop goes
    /// round again with no `break` (`notify-8.2.0/src/inotify.rs:367-374`).
    fn drain_read_failure() -> notify::Error {
        notify::Error::io(std::io::Error::other("input/output error"))
    }

    #[test]
    fn a_persistent_read_failure_is_reported_once_and_not_per_occurrence() {
        let (shared, filter, mut receiver) = harness();
        // Two flush windows, because per-window coalescing alone would still deliver
        // one `Overflow` per window — that is the rescan duty cycle this pins shut.
        for _ in 0..500 {
            ingest(&shared, &filter, None, Err(drain_read_failure()));
        }
        assert_eq!(
            delivered(&shared, &mut receiver),
            vec![WatchEvent::Overflow { dropped: 1 }],
            "500 iterations of one unrecoverable read must be one hole, not 500"
        );
        for _ in 0..500 {
            ingest(&shared, &filter, None, Err(drain_read_failure()));
        }
        assert!(
            delivered(&shared, &mut receiver).is_empty(),
            "the same unchanged failure must not re-report every flush window"
        );
        assert!(
            shared.losses.reported(),
            "the loss must stay readable after the event carrying it was consumed; \
             this is what `Watcher::lost_coverage` returns"
        );
    }

    #[test]
    fn the_report_floor_admits_a_repeat_only_once_it_has_elapsed() {
        let reports = ReportFloor::default();
        assert!(!reports.reported(), "nothing has gone wrong yet");
        let first = Instant::now();
        assert_eq!(reports.admit(first), Some(LossReport::First));
        assert_eq!(
            reports.admit(first + LOST_COVERAGE_REPORT_INTERVAL - Duration::from_millis(1)),
            None,
            "inside the floor the loss is already reported"
        );
        assert_eq!(
            reports.admit(first + LOST_COVERAGE_REPORT_INTERVAL),
            Some(LossReport::Again),
            "a watch that never recovers must be re-reported, not forgotten"
        );
        assert_eq!(
            reports.admit(first + LOST_COVERAGE_REPORT_INTERVAL),
            None,
            "the floor runs from the last admitted report, not from the first"
        );
        assert!(reports.reported());
    }

    /// A non-regression guard, not evidence for the report floor: it passes with the
    /// floor removed as well, because the error and the notice each contribute one
    /// hole either way. It exists to fail if the floor is ever widened to cover the
    /// rescan notice too. The floor's own behaviour is pinned by
    /// `the_floor_reopens_through_the_production_path_after_the_interval`.
    #[test]
    fn a_kernel_rescan_notice_is_not_suppressed_by_a_reported_error() {
        let (shared, filter, mut receiver) = harness();
        ingest(&shared, &filter, None, Err(drain_read_failure()));
        // A genuine, distinct loss arriving while the error's floor is still closed.
        // Rate-limiting this too would hide a hole the consumer has not rescanned
        // for, so the floor deliberately covers only the repeating error class.
        ingest(
            &shared,
            &filter,
            None,
            Ok(notify::Event::new(EventKind::Other).set_flag(Flag::Rescan)),
        );
        assert_eq!(
            delivered(&shared, &mut receiver),
            vec![WatchEvent::Overflow { dropped: 2 }],
            "a rescan notice must still count while a persistent error is suppressed"
        );
    }

    /// The exact input the reviewer measured: a read on the notification fd
    /// interrupted by a signal. `inotify-0.11.5` returns `io::Error::last_os_error()`
    /// without retrying `EINTR` (`src/inotify.rs:206-219`) and `notify` forwards
    /// every non-`WouldBlock` read error to the callback before looping round to read
    /// again, so this arrives here having lost nothing at all.
    fn interrupted_read() -> notify::Error {
        notify::Error::io(std::io::Error::from(std::io::ErrorKind::Interrupted))
    }

    /// The one loss an operator can actually fix.
    fn watch_limit_exhausted() -> notify::Error {
        notify::Error::new(NotifyErrorKind::MaxFilesWatch).add_path(PathBuf::from("/r/deep"))
    }

    #[test]
    fn an_interrupted_read_is_not_a_hole_and_does_not_latch() {
        let (shared, filter, mut receiver) = harness();
        ingest(&shared, &filter, None, Err(interrupted_read()));
        assert!(
            delivered(&shared, &mut receiver).is_empty(),
            "the events were still queued for the next read; nothing was lost"
        );
        assert!(
            !shared.losses.reported(),
            "a retried read must not leave the watch permanently marked degraded"
        );
    }

    #[test]
    fn an_interrupted_read_does_not_spend_the_watch_limit_report() {
        let (shared, filter, mut receiver) = harness();
        // Every value is measured before anything is asserted, so one run records
        // both halves of the defect: what the benign read did, and what the fifty
        // consequential failures behind it were then allowed to deliver.
        ingest(&shared, &filter, None, Err(interrupted_read()));
        let transient = delivered(&shared, &mut receiver);
        let latched = shared.losses.reported();
        for _ in 0..50 {
            ingest(&shared, &filter, None, Err(watch_limit_exhausted()));
        }
        let limit = delivered(&shared, &mut receiver);
        assert!(
            transient.is_empty() && !latched,
            "a benign interrupted read is not a coverage loss \
             (measured: it delivered {transient:?}, lost_coverage={latched}, and the \
             50 watch-limit failures behind it then delivered {limit:?})"
        );
        assert_eq!(
            limit,
            vec![WatchEvent::Overflow { dropped: 1 }],
            "the watch limit must still reach the consumer after a transient read error"
        );
        assert!(shared.losses.reported(), "this one really is lost coverage");
    }

    #[test]
    fn a_failing_drain_read_does_not_spend_the_watch_limit_report() {
        let (shared, filter, mut receiver) = harness();
        ingest(&shared, &filter, None, Err(drain_read_failure()));
        ingest(&shared, &filter, None, Err(watch_limit_exhausted()));
        assert_eq!(
            delivered(&shared, &mut receiver),
            vec![WatchEvent::Overflow { dropped: 2 }],
            "two different losses hold two different report budgets"
        );
    }

    #[test]
    fn only_a_read_the_backend_repeats_is_treated_as_no_loss() {
        use std::io::ErrorKind;

        assert_eq!(
            classify_failure(&notify::Error::io(std::io::Error::from(
                ErrorKind::Interrupted
            ))),
            WatchFailure::Retryable,
            "EINTR left every event in the kernel queue for the next read"
        );
        // The other kinds the same `read` can return. Each one either consumed events
        // or means the descriptor is finished, so the classification fails closed.
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::InvalidInput,
            ErrorKind::OutOfMemory,
            ErrorKind::PermissionDenied,
            ErrorKind::Other,
        ] {
            assert_eq!(
                classify_failure(&notify::Error::io(std::io::Error::from(kind))),
                WatchFailure::Lost(LossClass::Notifications),
                "{kind:?} is not a read the backend simply repeats"
            );
        }
        assert_eq!(
            classify_failure(&notify::Error::new(NotifyErrorKind::MaxFilesWatch)),
            WatchFailure::Lost(LossClass::WatchLimit),
            "the watch limit is budgeted on its own, away from every read error"
        );
        assert_eq!(
            classify_failure(&notify::Error::path_not_found()),
            WatchFailure::Incidental
        );
    }

    /// The public accessor, over the same state the callback thread writes.
    ///
    /// Checked through a real [`Watcher`] rather than through `Shared` because the
    /// accessor is the provider half of a capability whose consumer lives in another
    /// crate: if it stopped reflecting what the callback recorded, or started
    /// reflecting a read that lost nothing, this is the only place in this crate that
    /// would notice.
    #[test]
    fn the_public_accessor_reports_what_the_callback_recorded() {
        let (shared, filter, _receiver) = harness();
        let watcher = Watcher {
            decision: Decision::Full,
            shared: Arc::clone(&shared),
            filter: Arc::new(FilterBuilder::new("/r").build().expect("built-in patterns")),
            requested_root: PathBuf::from("/r"),
            active_scope: None,
            watch_missing_ancestors: false,
            inner: None,
            flush: None,
        };
        assert!(!watcher.lost_coverage(), "nothing has gone wrong yet");
        ingest(&shared, &filter, None, Err(interrupted_read()));
        assert!(
            !watcher.lost_coverage(),
            "a read the backend repeats took no coverage away"
        );
        ingest(&shared, &filter, None, Err(watch_limit_exhausted()));
        assert!(watcher.lost_coverage(), "an unwatched subtree did");
        assert_eq!(
            watcher.dropped(),
            0,
            "and it says so before the flush loop has published anything, which is \
             why a consumer can read it instead of remembering an `Overflow`"
        );
    }

    #[test]
    fn a_read_that_is_never_completed_fails_closed_into_a_loss() {
        let (quiet, filter, mut receiver) = harness();
        for _ in 0..RETRIED_READS_BEFORE_LOSS - 1 {
            ingest(&quiet, &filter, None, Err(interrupted_read()));
        }
        assert!(
            delivered(&quiet, &mut receiver).is_empty() && !quiet.losses.reported(),
            "below the threshold this is still a read the backend repeats"
        );

        let (stuck, filter, mut receiver) = harness();
        for _ in 0..RETRIED_READS_BEFORE_LOSS {
            ingest(&stuck, &filter, None, Err(interrupted_read()));
        }
        assert_eq!(
            delivered(&stuck, &mut receiver),
            vec![WatchEvent::Overflow { dropped: 1 }],
            "a read that is still being repeated with nothing delivered is a hole"
        );
        assert!(
            stuck.losses.reported(),
            "nothing is draining the kernel queue, and that queue is bounded"
        );
    }

    #[test]
    fn a_delivered_notification_resets_the_repeated_read_count() {
        let (shared, filter, mut receiver) = harness();
        for round in 0..3 {
            for _ in 0..RETRIED_READS_BEFORE_LOSS - 1 {
                ingest(&shared, &filter, None, Err(interrupted_read()));
            }
            ingest(
                &shared,
                &filter,
                None,
                Ok(event(
                    EventKind::Create(CreateKind::File),
                    &[&format!("/r/a{round}")],
                )),
            );
        }
        assert_eq!(
            delivered(&shared, &mut receiver),
            vec![
                WatchEvent::Changed(FileEvent::new("/r/a0", ChangeKind::Add)),
                WatchEvent::Changed(FileEvent::new("/r/a1", ChangeKind::Add)),
                WatchEvent::Changed(FileEvent::new("/r/a2", ChangeKind::Add)),
            ],
            "3071 interrupted reads that each ended in a delivery are not a hole"
        );
        assert!(
            !shared.losses.reported(),
            "the backend got through every time, so no coverage was lost"
        );
    }

    #[test]
    fn the_floor_reopens_through_the_production_path_after_the_interval() {
        let (shared, filter, mut receiver) = harness();
        let start = Instant::now();
        let feed = |at: Instant| {
            ingest_at(&shared, &filter, None, Err(watch_limit_exhausted()), at);
        };
        feed(start);
        feed(start + LOST_COVERAGE_REPORT_INTERVAL - Duration::from_millis(1));
        assert_eq!(
            delivered(&shared, &mut receiver),
            vec![WatchEvent::Overflow { dropped: 1 }],
            "inside the floor a repeat of the same condition adds nothing"
        );
        feed(start + LOST_COVERAGE_REPORT_INTERVAL);
        assert_eq!(
            delivered(&shared, &mut receiver),
            vec![WatchEvent::Overflow { dropped: 1 }],
            "a watch that never recovers must be re-reported once the floor elapses"
        );
        feed(start + LOST_COVERAGE_REPORT_INTERVAL + Duration::from_millis(1));
        assert!(
            delivered(&shared, &mut receiver).is_empty(),
            "the floor runs from the last admitted report, not from the first"
        );
    }

    #[test]
    fn an_error_about_one_missing_path_is_not_a_hole() {
        let (shared, filter, mut receiver) = harness();
        for error in [
            notify::Error::path_not_found(),
            notify::Error::watch_not_found(),
        ] {
            ingest(&shared, &filter, None, Err(error));
        }
        assert!(
            delivered(&shared, &mut receiver).is_empty(),
            "a short-lived directory must not cost the consumer a rescan"
        );
    }

    #[test]
    fn an_ordinary_change_still_publishes_without_an_overflow() {
        let (shared, filter, mut receiver) = harness();
        ingest(
            &shared,
            &filter,
            None,
            Ok(event(EventKind::Create(CreateKind::File), &["/r/a"])),
        );
        assert_eq!(
            delivered(&shared, &mut receiver),
            vec![WatchEvent::Changed(FileEvent::new("/r/a", ChangeKind::Add))]
        );
    }

    #[test]
    fn a_creation_is_an_add() {
        assert_eq!(
            classify(&event(EventKind::Create(CreateKind::File), &["/r/a"])),
            vec![(PathBuf::from("/r/a"), ChangeKind::Add)]
        );
    }

    #[test]
    fn a_data_write_is_a_change() {
        assert_eq!(
            classify(&event(
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                &["/r/a"]
            )),
            vec![(PathBuf::from("/r/a"), ChangeKind::Change)]
        );
    }

    #[test]
    fn a_removal_is_an_unlink() {
        assert_eq!(
            classify(&event(
                EventKind::Remove(notify::event::RemoveKind::File),
                &["/r/a"]
            )),
            vec![(PathBuf::from("/r/a"), ChangeKind::Unlink)]
        );
    }

    #[test]
    fn a_rename_reported_as_both_becomes_two_changes() {
        assert_eq!(
            classify(&event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/r/old", "/r/new"]
            )),
            vec![
                (PathBuf::from("/r/old"), ChangeKind::Unlink),
                (PathBuf::from("/r/new"), ChangeKind::Add),
            ]
        );
    }

    #[test]
    fn the_two_halves_of_a_split_rename_are_directed() {
        assert_eq!(
            classify(&event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/r/old"]
            )),
            vec![(PathBuf::from("/r/old"), ChangeKind::Unlink)]
        );
        assert_eq!(
            classify(&event(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &["/r/new"]
            )),
            vec![(PathBuf::from("/r/new"), ChangeKind::Add)]
        );
    }

    #[test]
    fn an_access_is_not_a_change() {
        assert!(
            classify(&event(
                EventKind::Access(notify::event::AccessKind::Close(
                    notify::event::AccessMode::Write
                )),
                &["/r/a"]
            ))
            .is_empty(),
            "the oracle publishes only create/update/delete"
        );
    }

    #[test]
    fn an_undirected_or_unknown_kind_reads_as_a_change() {
        for kind in [
            EventKind::Any,
            EventKind::Other,
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
        ] {
            assert_eq!(
                classify(&event(kind, &["/r/a"])),
                vec![(PathBuf::from("/r/a"), ChangeKind::Change)],
                "{kind:?} must not be read as a removal"
            );
        }
    }

    #[test]
    fn only_head_is_reportable_inside_the_vcs_directory() {
        let vcs = Path::new("/r/.git");
        assert!(is_vcs_reportable(vcs, Path::new("/r/.git/HEAD")));
        assert!(!is_vcs_reportable(vcs, Path::new("/r/.git/index")));
        assert!(!is_vcs_reportable(
            vcs,
            Path::new("/r/.git/refs/heads/main")
        ));
    }

    #[test]
    fn the_documented_defaults_are_the_ones_in_use() {
        let options = WatchOptions::new("/r");
        assert_eq!(options.capacity, DEFAULT_CAPACITY);
        assert_eq!(options.max_pending, DEFAULT_MAX_PENDING);
        assert_eq!(options.debounce, DEFAULT_DEBOUNCE);
        assert_eq!(options.max_wait, DEFAULT_MAX_WAIT);
        assert_eq!(
            DEFAULT_MAX_PENDING,
            DEFAULT_CAPACITY * 4,
            "the buffer is documented as four flush-windows deep"
        );
    }

    #[test]
    fn a_zero_capacity_is_raised_to_one() {
        let options = WatchOptions::new("/r").capacity(0).max_pending(0);
        assert_eq!(options.capacity, 1);
        assert_eq!(options.max_pending, 1);
    }
}
