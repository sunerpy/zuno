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
//! # Why the watcher thread cannot be blocked
//!
//! [`notify`] runs its callback on a thread it owns. Blocking there stops the
//! inotify queue being drained, and the kernel's queue is itself bounded
//! (`/proc/sys/fs/inotify/max_queued_events`, 16384 by default) — overrun it and
//! the kernel sets `IN_Q_OVERFLOW` and *silently discards* events. So the
//! callback only ever takes a mutex around the coalescing buffer, and the only
//! send it performs is [`tokio::sync::mpsc::Sender::try_send`], from a different
//! thread. No path in this crate calls a blocking or awaiting send.
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
    /// number without keeping a running total.
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

/// What the callback thread and the flush thread share.
struct Shared {
    debouncer: Mutex<Debouncer>,
    wake: Condvar,
    shutdown: AtomicBool,
    sender: Sender<WatchEvent>,
    counters: Counters,
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

    /// Paths discarded without being reported so far.
    ///
    /// Mirrors the totals delivered as [`WatchEvent::Overflow`]; non-zero means a
    /// consumer's view of the tree has a hole.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.counters.dropped.load(Ordering::Relaxed)
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
/// filter, take a mutex, merge, wake. No I/O, no send, no allocation beyond the
/// paths `notify` already allocated.
fn ingest(
    shared: &Arc<Shared>,
    filter: &Filter,
    vcs_dir: Option<&Path>,
    event: notify::Result<notify::Event>,
) {
    let event = match event {
        Ok(event) => event,
        Err(error) => {
            // A watch error is not a change, and the oracle's callback likewise
            // ignores its `_error` parameter (`watcher.ts:83`).
            tracing::debug!(%error, "watch error");
            return;
        }
    };
    let now = Instant::now();
    let mut woke = false;
    let mut guard = shared.lock();
    for (path, kind) in classify(&event) {
        let reportable = match vcs_dir {
            Some(dir) if path.starts_with(dir) => is_vcs_reportable(dir, &path),
            _ => !filter.is_ignored(&path, kind != ChangeKind::Unlink && path.is_dir()),
        };
        if !reportable {
            continue;
        }
        guard.accept(path, kind, now);
        woke = true;
    }
    shared
        .counters
        .accepted
        .store(guard.accepted(), Ordering::Relaxed);
    drop(guard);
    if woke {
        shared.wake.notify_one();
    }
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
    use notify::event::{CreateKind, DataChange, ModifyKind};

    fn event(kind: EventKind, paths: &[&str]) -> notify::Event {
        notify::Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: notify::event::EventAttributes::new(),
        }
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
