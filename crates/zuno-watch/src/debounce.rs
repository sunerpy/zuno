//! Coalescing: many filesystem notifications in, at most one event per path out.
//!
//! # What a raw notification stream looks like
//!
//! One `write` of one file is not one notification. On Linux, creating and
//! writing `a.txt` yields `Create(File)`, then `Modify(Data(Any))`, then a
//! `Modify(Metadata(..))` or `Access(Close(Write))` depending on kernel version
//! and how the file was opened. The oracle publishes one event per notification
//! (`filesystem/watcher.ts:85-91`), so a consumer that re-reads a file on every
//! event re-reads it three times for one save. Coalescing exists to make that
//! one.
//!
//! # The merge rule
//!
//! Within one window a path collapses to a single kind. The rule is *the newer
//! kind wins, except that [`ChangeKind::Add`] survives a following
//! [`ChangeKind::Change`]*:
//!
//! | older    | newer    | result   | why |
//! |----------|----------|----------|-----|
//! | `Add`    | `Change` | `Add`    | the consumer has never heard of this path, so the first thing it must hear is "this is new" |
//! | `Add`    | `Unlink` | `Unlink` | net-removed; an `Unlink` for a path a consumer never had is a no-op, whereas emitting nothing would leave a consumer that *did* observe the file mid-window stale |
//! | `Change` | `Add`    | `Add`    | delete-then-create, i.e. the atomic-rename save every editor does |
//! | `Change` | `Unlink` | `Unlink` | gone |
//! | `Unlink` | `Add`    | `Add`    | same atomic-rename shape, seen in the other order |
//! | `Unlink` | `Change` | `Change` | the path exists again and has content to read |
//!
//! Every consumer action is idempotent under this rule: `Add` and `Change` both
//! mean "read this path", `Unlink` means "forget it". That is the property that
//! makes collapsing safe — the oracle's three-event sequence and this one event
//! leave a consumer in the same state.
//!
//! # Why time is an argument
//!
//! Nothing here calls [`Instant::now`]. Every entry point takes the current
//! instant, so the whole state machine is a pure function of an explicit
//! timeline and its unit tests need no `sleep` and cannot flake. The one place
//! that reads the clock is the flush loop in [`crate::Watcher`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What happened to a path (`schema/src/filesystem-watcher.ts:9`).
///
/// The three names are the oracle's `"add" | "change" | "unlink"` literals, which
/// are themselves `@parcel/watcher`'s `create`/`update`/`delete` renamed at
/// `filesystem/watcher.ts:86-89`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeKind {
    /// The path came into existence. `"add"`.
    Add,
    /// The path's contents or metadata changed. `"change"`.
    Change,
    /// The path went away. `"unlink"`.
    Unlink,
}

impl ChangeKind {
    /// The oracle's literal for this kind, for a consumer that re-serialises the
    /// `file.watcher.updated` event.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Change => "change",
            Self::Unlink => "unlink",
        }
    }

    /// Collapse an older and a newer observation of the same path.
    ///
    /// See the module docs for the reasoning behind the one asymmetric case.
    #[must_use]
    pub fn merge(older: Self, newer: Self) -> Self {
        match (older, newer) {
            (Self::Add, Self::Change) => Self::Add,
            (_, newer) => newer,
        }
    }
}

/// One coalesced change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEvent {
    /// The absolute path that changed.
    pub path: PathBuf,
    /// What happened to it, after merging everything seen in the window.
    pub kind: ChangeKind,
}

impl FileEvent {
    /// Construct an event.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, kind: ChangeKind) -> Self {
        Self {
            path: path.into(),
            kind,
        }
    }
}

/// What one flush produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flush {
    /// The coalesced events, in path order.
    ///
    /// Sorted because the backing map is a [`BTreeMap`]: a stable order makes a
    /// transcript diffable and makes "the first N" a meaningful phrase, at no
    /// cost over a hash map at these sizes. `zuno-search` sorts for the same reason.
    pub events: Vec<FileEvent>,
    /// How many paths were discarded, unreported, since the last flush.
    ///
    /// Non-zero only when the pending map hit its ceiling. A consumer that sees
    /// this knows its view of the tree has a hole in it and must rescan; that is
    /// the whole reason the number is surfaced rather than logged.
    pub dropped: u64,
}

impl Flush {
    /// Whether this flush carries nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.dropped == 0
    }
}

/// The coalescing buffer.
///
/// Holds at most one entry per path, and at most [`Debouncer::max_pending`]
/// entries in total. Both bounds are what make "never grow" true rather than
/// aspirational: path-keyed coalescing alone bounds the buffer by the number of
/// *distinct* paths in a window, which a `find / -exec touch` makes unbounded.
#[derive(Debug)]
pub struct Debouncer {
    quiet: Duration,
    max_wait: Duration,
    max_pending: usize,
    pending: BTreeMap<PathBuf, ChangeKind>,
    window_opened: Option<Instant>,
    last_activity: Option<Instant>,
    accepted: u64,
    dropped: u64,
}

impl Debouncer {
    /// Build a buffer.
    ///
    /// `quiet` is the trailing debounce: a flush becomes due once nothing new has
    /// arrived for that long. `max_wait` caps how long the oldest pending change
    /// can be held when activity never stops — without it, a `cargo build` that
    /// touches something every 50 ms would starve the consumer for the duration
    /// of the build. `max_pending` is the hard ceiling on distinct paths held.
    #[must_use]
    pub fn new(quiet: Duration, max_wait: Duration, max_pending: usize) -> Self {
        Self {
            quiet,
            max_wait,
            // A zero ceiling would drop everything and is never what a caller
            // means; one entry is the smallest coherent buffer.
            max_pending: max_pending.max(1),
            pending: BTreeMap::new(),
            window_opened: None,
            last_activity: None,
            accepted: 0,
            dropped: 0,
        }
    }

    /// The ceiling on distinct pending paths.
    #[must_use]
    pub fn max_pending(&self) -> usize {
        self.max_pending
    }

    /// How many raw notifications have been folded in.
    ///
    /// Compared against the number of events actually delivered, this is the
    /// measurement that shows coalescing happened at all.
    #[must_use]
    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    /// How many distinct paths are currently held.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Fold one notification in.
    ///
    /// A path already pending is merged in place, which is why a burst of
    /// notifications for one file costs one map entry rather than N. Once the map
    /// is full, a notification for a **new** path is dropped and counted;
    /// notifications for paths already held are still merged, because merging
    /// costs no memory and losing them would report stale kinds.
    pub fn accept(&mut self, path: PathBuf, kind: ChangeKind, now: Instant) {
        self.accepted = self.accepted.saturating_add(1);
        self.last_activity = Some(now);
        if self.window_opened.is_none() {
            self.window_opened = Some(now);
        }
        match self.pending.get_mut(&path) {
            Some(existing) => *existing = ChangeKind::merge(*existing, kind),
            None => {
                if self.pending.len() >= self.max_pending {
                    self.dropped = self.dropped.saturating_add(1);
                    return;
                }
                self.pending.insert(path, kind);
            }
        }
    }

    /// Put previously flushed events back, as the *older* observation.
    ///
    /// Used when the publish channel refused a batch: the events have not been
    /// seen by anyone, so they belong back in the buffer — but anything that
    /// arrived while the send was attempted is newer and must win. The same
    /// ceiling applies, so a consumer that never drains cannot make this grow.
    pub fn requeue<I: IntoIterator<Item = FileEvent>>(&mut self, events: I, now: Instant) {
        for event in events {
            match self.pending.get_mut(&event.path) {
                Some(newer) => *newer = ChangeKind::merge(event.kind, *newer),
                None => {
                    if self.pending.len() >= self.max_pending {
                        self.dropped = self.dropped.saturating_add(1);
                        continue;
                    }
                    self.pending.insert(event.path, event.kind);
                }
            }
        }
        // `last_activity` as well as `window_opened`, so the next deadline is one
        // quiet period out. That is what gives the publish side its backoff: a
        // refused batch is retried after `quiet`, not immediately in a hot loop.
        self.window_opened = Some(self.window_opened.unwrap_or(now));
        self.last_activity = Some(now);
    }

    /// Record that `count` events were dropped for a reason outside this buffer.
    ///
    /// The publish side needs to fold its own losses into the same counter, so a
    /// consumer sees one "you missed N" number rather than two it has to add up.
    /// Opens the window if nothing else has, because a loss with no surviving
    /// event still has to reach the consumer.
    pub fn record_dropped(&mut self, count: u64, now: Instant) {
        self.dropped = self.dropped.saturating_add(count);
        self.window_opened = Some(self.window_opened.unwrap_or(now));
        self.last_activity = Some(self.last_activity.unwrap_or(now));
    }

    /// When the next flush becomes due, or `None` when there is nothing to flush.
    ///
    /// The earlier of "quiet since the last notification" and "`max_wait` since
    /// the window opened".
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        if self.pending.is_empty() && self.dropped == 0 {
            return None;
        }
        let quiet_until = self.last_activity.map(|at| at + self.quiet);
        let hard_until = self.window_opened.map(|at| at + self.max_wait);
        match (quiet_until, hard_until) {
            (Some(quiet), Some(hard)) => Some(quiet.min(hard)),
            (quiet, hard) => quiet.or(hard),
        }
    }

    /// Whether a flush is due at `now`.
    #[must_use]
    pub fn due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }

    /// Take everything pending, resetting the window.
    #[must_use]
    pub fn flush(&mut self) -> Flush {
        let events = std::mem::take(&mut self.pending)
            .into_iter()
            .map(|(path, kind)| FileEvent { path, kind })
            .collect();
        let dropped = std::mem::take(&mut self.dropped);
        self.window_opened = None;
        self.last_activity = None;
        Flush { events, dropped }
    }

    /// Whether a path is currently held, for assertions and diagnostics.
    #[must_use]
    pub fn peek(&self, path: &Path) -> Option<ChangeKind> {
        self.pending.get(path).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    fn debouncer() -> Debouncer {
        Debouncer::new(Duration::from_millis(50), Duration::from_millis(500), 4)
    }

    #[test]
    fn kind_literals_match_the_schema() {
        assert_eq!(ChangeKind::Add.as_str(), "add");
        assert_eq!(ChangeKind::Change.as_str(), "change");
        assert_eq!(ChangeKind::Unlink.as_str(), "unlink");
    }

    #[test]
    fn add_survives_a_following_change() {
        assert_eq!(
            ChangeKind::merge(ChangeKind::Add, ChangeKind::Change),
            ChangeKind::Add
        );
    }

    #[test]
    fn every_other_pair_takes_the_newer_kind() {
        use ChangeKind::{Add, Change, Unlink};
        for (older, newer) in [
            (Add, Unlink),
            (Change, Add),
            (Change, Unlink),
            (Unlink, Add),
            (Unlink, Change),
            (Add, Add),
            (Change, Change),
            (Unlink, Unlink),
        ] {
            assert_eq!(
                ChangeKind::merge(older, newer),
                newer,
                "{older:?}+{newer:?}"
            );
        }
    }

    #[test]
    fn merging_is_idempotent_for_a_repeated_kind() {
        for kind in [ChangeKind::Add, ChangeKind::Change, ChangeKind::Unlink] {
            assert_eq!(ChangeKind::merge(kind, kind), kind);
        }
    }

    #[test]
    fn one_save_worth_of_notifications_becomes_one_event() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        // The real inotify sequence for creating and writing a file.
        debouncer.accept("/r/a".into(), ChangeKind::Add, at(base, 0));
        debouncer.accept("/r/a".into(), ChangeKind::Change, at(base, 1));
        debouncer.accept("/r/a".into(), ChangeKind::Change, at(base, 2));
        assert_eq!(debouncer.accepted(), 3);
        assert_eq!(debouncer.pending_len(), 1);
        let flush = debouncer.flush();
        assert_eq!(
            flush.events,
            vec![FileEvent::new("/r/a", ChangeKind::Add)],
            "three notifications, one event, still reported as a creation"
        );
        assert_eq!(flush.dropped, 0);
    }

    #[test]
    fn create_then_delete_in_one_window_reports_the_deletion() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        debouncer.accept("/r/tmp1".into(), ChangeKind::Add, at(base, 0));
        debouncer.accept("/r/tmp1".into(), ChangeKind::Change, at(base, 1));
        debouncer.accept("/r/tmp1".into(), ChangeKind::Unlink, at(base, 2));
        assert_eq!(
            debouncer.flush().events,
            vec![FileEvent::new("/r/tmp1", ChangeKind::Unlink)]
        );
    }

    #[test]
    fn an_atomic_rename_save_reports_a_creation() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        debouncer.accept("/r/a".into(), ChangeKind::Unlink, at(base, 0));
        debouncer.accept("/r/a".into(), ChangeKind::Add, at(base, 1));
        assert_eq!(
            debouncer.flush().events,
            vec![FileEvent::new("/r/a", ChangeKind::Add)]
        );
    }

    #[test]
    fn flushed_events_are_path_sorted() {
        let base = Instant::now();
        let mut debouncer = Debouncer::new(Duration::from_millis(50), Duration::from_secs(1), 16);
        for name in ["/r/c", "/r/a", "/r/b"] {
            debouncer.accept(name.into(), ChangeKind::Change, base);
        }
        let paths: Vec<_> = debouncer
            .flush()
            .events
            .into_iter()
            .map(|event| event.path)
            .collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/r/a"),
                PathBuf::from("/r/b"),
                PathBuf::from("/r/c")
            ]
        );
    }

    #[test]
    fn nothing_pending_means_no_deadline() {
        let mut debouncer = debouncer();
        assert!(debouncer.deadline().is_none());
        assert!(!debouncer.due(Instant::now()));
        assert!(debouncer.flush().is_empty());
    }

    #[test]
    fn the_quiet_period_gates_the_flush() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        debouncer.accept("/r/a".into(), ChangeKind::Change, base);
        assert!(!debouncer.due(at(base, 49)));
        assert!(debouncer.due(at(base, 50)));
    }

    #[test]
    fn each_notification_pushes_the_quiet_deadline_out() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        debouncer.accept("/r/a".into(), ChangeKind::Change, base);
        debouncer.accept("/r/b".into(), ChangeKind::Change, at(base, 40));
        assert!(!debouncer.due(at(base, 80)));
        assert!(debouncer.due(at(base, 90)));
    }

    #[test]
    fn max_wait_flushes_under_unending_activity() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        // Something arrives every 10 ms forever, so the 50 ms quiet period never
        // elapses. Without `max_wait` the consumer would starve.
        for step in 0..200 {
            debouncer.accept("/r/a".into(), ChangeKind::Change, at(base, step * 10));
        }
        assert!(
            debouncer.due(at(base, 500)),
            "the 500 ms hard cap must fire even though nothing was ever quiet"
        );
    }

    #[test]
    fn the_pending_map_never_exceeds_its_ceiling() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        for index in 0..1_000 {
            debouncer.accept(format!("/r/{index}").into(), ChangeKind::Add, base);
            assert!(debouncer.pending_len() <= debouncer.max_pending());
        }
        let flush = debouncer.flush();
        assert_eq!(flush.events.len(), 4, "the ceiling, not the burst size");
        assert_eq!(flush.dropped, 996);
        assert_eq!(debouncer.accepted(), 1_000);
    }

    #[test]
    fn a_full_buffer_still_merges_paths_it_already_holds() {
        let base = Instant::now();
        let mut debouncer = Debouncer::new(Duration::from_millis(50), Duration::from_secs(1), 1);
        debouncer.accept("/r/a".into(), ChangeKind::Add, base);
        debouncer.accept("/r/b".into(), ChangeKind::Add, base);
        // `/r/a` is held, so its deletion must land even though the map is full.
        debouncer.accept("/r/a".into(), ChangeKind::Unlink, base);
        let flush = debouncer.flush();
        assert_eq!(
            flush.events,
            vec![FileEvent::new("/r/a", ChangeKind::Unlink)]
        );
        assert_eq!(flush.dropped, 1);
    }

    #[test]
    fn a_zero_ceiling_is_raised_to_one() {
        assert_eq!(
            Debouncer::new(Duration::from_millis(1), Duration::from_millis(1), 0).max_pending(),
            1
        );
    }

    #[test]
    fn requeued_events_lose_to_whatever_arrived_since() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        debouncer.accept("/r/a".into(), ChangeKind::Unlink, base);
        // The flushed `Add` is older than the `Unlink` now pending.
        debouncer.requeue([FileEvent::new("/r/a", ChangeKind::Add)], at(base, 1));
        assert_eq!(debouncer.peek(Path::new("/r/a")), Some(ChangeKind::Unlink));
    }

    #[test]
    fn requeued_events_survive_when_nothing_newer_arrived() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        debouncer.requeue([FileEvent::new("/r/a", ChangeKind::Add)], base);
        assert!(
            debouncer.deadline().is_some(),
            "a requeue reopens the window"
        );
        assert_eq!(
            debouncer.flush().events,
            vec![FileEvent::new("/r/a", ChangeKind::Add)]
        );
    }

    #[test]
    fn a_requeue_respects_the_same_ceiling() {
        let base = Instant::now();
        let mut debouncer = Debouncer::new(Duration::from_millis(50), Duration::from_secs(1), 2);
        let batch: Vec<_> = (0..10)
            .map(|index| FileEvent::new(format!("/r/{index}"), ChangeKind::Add))
            .collect();
        debouncer.requeue(batch, base);
        assert_eq!(debouncer.pending_len(), 2);
        assert_eq!(debouncer.flush().dropped, 8);
    }

    #[test]
    fn a_recorded_drop_alone_still_produces_a_flush() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        debouncer.record_dropped(7, base);
        assert!(
            debouncer.due(at(base, 50)),
            "a loss with no surviving event must still become due"
        );
        let flush = debouncer.flush();
        assert!(flush.events.is_empty());
        assert_eq!(
            flush.dropped, 7,
            "a loss must reach the consumer on its own"
        );
    }

    #[test]
    fn a_requeue_backs_off_by_one_quiet_period() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        debouncer.requeue([FileEvent::new("/r/a", ChangeKind::Add)], base);
        assert!(
            !debouncer.due(at(base, 49)),
            "retrying a refused batch immediately would spin the flush loop"
        );
        assert!(debouncer.due(at(base, 50)));
    }

    #[test]
    fn flushing_resets_the_window() {
        let base = Instant::now();
        let mut debouncer = debouncer();
        debouncer.accept("/r/a".into(), ChangeKind::Change, base);
        drop(debouncer.flush());
        assert!(debouncer.deadline().is_none());
        assert_eq!(debouncer.pending_len(), 0);
    }
}
