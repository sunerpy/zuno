//! Live-filesystem tests for [`oc_watch::Watcher`].
//!
//! # How these stay deterministic
//!
//! Filesystem-watching tests are the canonical source of flaky suites, so none of
//! these use a fixed `sleep` as a synchronisation primitive. Every wait is
//! [`poll_until`]: a condition checked on a short interval against a hard
//! deadline, failing with the observed state rather than a bare timeout. Every
//! test also shortens the debounce, so waiting for a real flush costs hundreds of
//! milliseconds rather than the production second.
//!
//! Two knobs exist purely so the pressure paths are reachable without generating
//! luck-dependent load: [`WatchOptions::capacity`] and
//! [`WatchOptions::max_pending`]. Lowering them makes the coalesce-and-drop policy
//! observable in under a second, which is the difference between testing it and
//! asserting it in a comment.
//!
//! # Platform
//!
//! Run on Linux, where `notify` uses inotify. Two behaviours below are
//! inotify-specific and noted where they matter: writing a file yields several
//! notifications rather than one, and `IN_CLOSE_WRITE` arrives as
//! `EventKind::Access`, which this crate discards.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oc_watch::{
    ChangeKind, Decision, DisabledReason, EventStream, WatchEvent, WatchOptions, Watcher,
};

/// How often a poll re-checks its condition.
const POLL_STEP: Duration = Duration::from_millis(5);

/// Trailing debounce used by tests that want exactly one flush.
///
/// Long enough to swallow the whole write phase of a thousand-file burst, so the
/// burst produces one window rather than a number of windows that depends on how
/// fast the host's disk is.
const TEST_DEBOUNCE: Duration = Duration::from_millis(250);

/// Poll `ready` until it is true or `budget` elapses.
///
/// Returns whether it became true. Callers assert on the return value with a
/// message naming what they were waiting for, so a timeout reads as a statement
/// about the system rather than "the sleep was too short".
fn poll_until(budget: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_STEP);
    }
}

/// Drain until nothing new arrives for `quiet`, or `budget` elapses.
///
/// This is the deterministic replacement for "sleep a bit and hope the events
/// landed": it returns only once the stream has actually gone quiet, and the
/// caller can tell a genuine quiescence from a timeout.
fn drain_until_quiet(
    stream: &mut EventStream,
    quiet: Duration,
    budget: Duration,
) -> (Vec<WatchEvent>, bool) {
    let mut collected = Vec::new();
    let hard_deadline = Instant::now() + budget;
    let mut quiet_deadline = Instant::now() + quiet;
    loop {
        match stream.try_recv() {
            Some(event) => {
                collected.push(event);
                quiet_deadline = Instant::now() + quiet;
            }
            None => {
                let now = Instant::now();
                if now >= quiet_deadline {
                    return (collected, true);
                }
                if now >= hard_deadline {
                    return (collected, false);
                }
                std::thread::sleep(POLL_STEP);
            }
        }
    }
}

/// The changes in `events`, keyed by path, plus how many events each path caused.
fn tally(events: &[WatchEvent]) -> (BTreeMap<PathBuf, ChangeKind>, BTreeMap<PathBuf, usize>) {
    let mut kinds = BTreeMap::new();
    let mut counts: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for event in events {
        if let WatchEvent::Changed(change) = event {
            kinds.insert(change.path.clone(), change.kind);
            *counts.entry(change.path.clone()).or_default() += 1;
        }
    }
    (kinds, counts)
}

/// Total paths reported as dropped by every [`WatchEvent::Overflow`] seen.
fn overflow_total(events: &[WatchEvent]) -> u64 {
    events
        .iter()
        .filter_map(|event| match event {
            WatchEvent::Overflow { dropped } => Some(*dropped),
            WatchEvent::Changed(_) => None,
        })
        .sum()
}

/// This process's resident set size in KiB, from `/proc/self/status`.
///
/// Read directly rather than through `oc-testkit`, whose equivalent
/// (`perf/process_tree.rs`) is `pub(crate)` and measures a *child* process tree;
/// this needs the test's own RSS, and the parse is four lines. `None` on a kernel
/// or container that omits `VmRSS`, which callers treat as "cannot measure" rather
/// than as a failure.
fn rss_kib() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

/// A watch over a fresh temporary directory, already running.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    watcher: Watcher,
    stream: EventStream,
}

impl Fixture {
    /// Start a watch, waiting until inotify has actually registered it.
    ///
    /// `notify::Watcher::watch` returns before the kernel is guaranteed to be
    /// delivering, so every fixture writes one probe file and blocks until it is
    /// seen. Without this the first assertion in a test races the watch
    /// registration — the single most common source of flakiness in this kind of
    /// suite.
    fn start(configure: impl FnOnce(WatchOptions) -> WatchOptions) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonical root");
        let options = configure(
            WatchOptions::new(&root)
                .env(oc_paths::Env::empty().with("ZUNO_EXPERIMENTAL_FILEWATCHER", "true"))
                .debounce(TEST_DEBOUNCE)
                .max_wait(Duration::from_secs(30)),
        );
        let (watcher, mut stream) = Watcher::start(options).expect("watch starts");

        let probe = root.join(".oc-watch-probe");
        let mut armed = false;
        for _ in 0..40 {
            fs::write(&probe, b"probe").expect("probe write");
            if poll_until(Duration::from_millis(400), || {
                stream.try_recv().is_some() || watcher.accepted() > 0
            }) {
                armed = true;
                break;
            }
        }
        assert!(armed, "inotify never reported the probe write");
        fs::remove_file(&probe).expect("probe removal");
        // Let the probe's own events flush and be discarded, so the test body
        // starts from a stream carrying nothing but its own writes.
        let _probe_noise =
            drain_until_quiet(&mut stream, TEST_DEBOUNCE * 3, Duration::from_secs(5));
        Self {
            _dir: dir,
            root,
            watcher,
            stream,
        }
    }
}

#[test]
fn editing_one_file_produces_exactly_one_coalesced_change_event() {
    let mut fixture = Fixture::start(|options| options);
    let file = fixture.root.join("edited.txt");
    fs::write(&file, b"first").expect("write");

    let (events, quiet) = drain_until_quiet(
        &mut fixture.stream,
        TEST_DEBOUNCE * 3,
        Duration::from_secs(10),
    );
    assert!(quiet, "the stream never went quiet: {events:?}");

    let (kinds, counts) = tally(&events);
    assert_eq!(
        counts.get(&file).copied(),
        Some(1),
        "one save must be one event, got {events:?}"
    );
    assert_eq!(
        kinds.get(&file).copied(),
        Some(ChangeKind::Add),
        "a file that did not exist before must be reported as an addition"
    );
    assert_eq!(
        overflow_total(&events),
        0,
        "one file cannot overflow anything"
    );
    // The coalescing claim, stated as a measurement: inotify produced more
    // notifications for this one write than the consumer received events.
    assert!(
        fixture.watcher.accepted() > fixture.watcher.published(),
        "no coalescing happened: accepted={} published={}",
        fixture.watcher.accepted(),
        fixture.watcher.published()
    );
    eprintln!(
        "single edit: accepted={} published={} events={}",
        fixture.watcher.accepted(),
        fixture.watcher.published(),
        events.len()
    );
}

#[test]
fn a_burst_of_one_thousand_files_is_coalesced_to_one_event_per_path() {
    /// The burst size the acceptance criterion names.
    const FILES: usize = 1_000;
    /// The documented bound on delivered events.
    ///
    /// Coalescing is keyed by path, so the floor is one event per distinct path
    /// and there is nothing below it to reach: a thousand *different* files carry
    /// a thousand independent facts. The bound therefore is `FILES`, and what the
    /// test proves is that the delivered count sits at that floor while the raw
    /// notification count sits well above it.
    const DELIVERED_BOUND: usize = FILES;
    /// Ceiling on the RSS growth the burst may cause, in KiB.
    ///
    /// Corroborates the structural bounds rather than replacing them: RSS is a
    /// page-granular instrument and the buffers here are tens of KiB, so the
    /// assertions that carry the weight are `pending <= max_pending` and
    /// `queued <= capacity`. This catches the failure mode those would not — a
    /// leak somewhere else in the pipeline.
    const RSS_CEILING_KIB: u64 = 8 * 1024;

    let mut fixture = Fixture::start(|options| options);
    let baseline = rss_kib();
    let mut peak = baseline;

    let expected: Vec<PathBuf> = (0..FILES)
        .map(|index| fixture.root.join(format!("burst-{index:04}.txt")))
        .collect();
    for path in &expected {
        fs::write(path, b"x").expect("burst write");
        // Structural invariant, checked *during* the burst rather than after: the
        // coalescing buffer may never exceed its ceiling, whatever the load.
        assert!(
            fixture.watcher.pending() <= oc_watch::DEFAULT_MAX_PENDING,
            "pending {} exceeded the ceiling {}",
            fixture.watcher.pending(),
            oc_watch::DEFAULT_MAX_PENDING
        );
        assert!(
            fixture.stream.queued() <= fixture.stream.capacity(),
            "queued {} exceeded the bounded channel's {} slots",
            fixture.stream.queued(),
            fixture.stream.capacity()
        );
        if let (Some(current), Some(high)) = (rss_kib(), peak) {
            peak = Some(current.max(high));
        }
    }

    let (events, quiet) = drain_until_quiet(
        &mut fixture.stream,
        TEST_DEBOUNCE * 3,
        Duration::from_secs(30),
    );
    assert!(quiet, "the stream never went quiet after the burst");
    if let (Some(current), Some(high)) = (rss_kib(), peak) {
        peak = Some(current.max(high));
    }

    let (kinds, counts) = tally(&events);
    let delivered = counts.values().sum::<usize>();
    let accepted = fixture.watcher.accepted();
    eprintln!(
        "burst: files={FILES} raw_notifications_accepted={accepted} \
         delivered_events={delivered} distinct_paths={} bound={DELIVERED_BOUND} \
         overflow_dropped={} rss_baseline_kib={:?} rss_peak_kib={:?}",
        counts.len(),
        overflow_total(&events),
        baseline,
        peak
    );

    assert_eq!(
        overflow_total(&events),
        0,
        "the default 4096-path buffer must absorb a 1000-file burst without dropping"
    );
    let missing: Vec<_> = expected
        .iter()
        .filter(|path| !kinds.contains_key(*path))
        .take(5)
        .collect();
    assert!(
        missing.is_empty(),
        "{} of {FILES} files were never reported, first few: {missing:?}",
        expected.iter().filter(|p| !kinds.contains_key(*p)).count()
    );
    assert!(
        delivered <= DELIVERED_BOUND,
        "delivered {delivered} events for {FILES} files, above the documented bound \
         {DELIVERED_BOUND}; per-path counts above 1: {:?}",
        counts
            .iter()
            .filter(|(_, count)| **count > 1)
            .take(5)
            .collect::<Vec<_>>()
    );
    assert!(
        accepted > delivered as u64,
        "no coalescing: {accepted} raw notifications produced {delivered} events"
    );

    match (baseline, peak) {
        (Some(baseline), Some(peak)) => {
            let delta = peak.saturating_sub(baseline);
            assert!(
                delta <= RSS_CEILING_KIB,
                "RSS grew {delta} KiB during the burst, over the {RSS_CEILING_KIB} KiB ceiling"
            );
            eprintln!("burst: rss_delta_kib={delta} ceiling_kib={RSS_CEILING_KIB}");
        }
        _ => eprintln!("burst: rss unavailable on this kernel, structural bounds still asserted"),
    }
}

#[test]
fn a_stalled_consumer_causes_coalescing_and_a_visible_drop_not_growth() {
    /// Enough files that an unbounded queue would hold thousands of entries.
    const FILES: usize = 4_000;
    /// Deliberately tiny, so the give-up path is reached in under a second
    /// instead of needing a burst large enough to hit the production ceiling.
    const CAPACITY: usize = 16;
    /// Likewise tiny. Total memory the pipeline may hold is therefore
    /// `CAPACITY + MAX_PENDING` events, whatever the burst size.
    const MAX_PENDING: usize = 64;
    /// Ceiling on RSS growth while 4,000 changes arrive and nothing is read.
    const RSS_CEILING_KIB: u64 = 8 * 1024;

    let mut fixture = Fixture::start(|options| {
        options
            .capacity(CAPACITY)
            .max_pending(MAX_PENDING)
            // Short, so several flushes are attempted against the full channel
            // and the requeue-then-drop path is exercised repeatedly.
            .debounce(Duration::from_millis(20))
            .max_wait(Duration::from_millis(100))
    });
    let baseline = rss_kib();
    let mut peak = baseline;

    // The consumer is stalled for the whole burst: nothing calls `try_recv`.
    for index in 0..FILES {
        fs::write(fixture.root.join(format!("stall-{index:04}.txt")), b"x").expect("write");
        assert!(
            fixture.watcher.pending() <= MAX_PENDING,
            "pending {} exceeded the {MAX_PENDING}-path ceiling at file {index}",
            fixture.watcher.pending()
        );
        assert!(
            fixture.stream.queued() <= CAPACITY,
            "queued {} exceeded the {CAPACITY}-slot channel at file {index}",
            fixture.stream.queued()
        );
        if let (Some(current), Some(high)) = (rss_kib(), peak) {
            peak = Some(current.max(high));
        }
    }
    // Give the flush loop time to attempt sends against the full channel.
    assert!(
        poll_until(Duration::from_secs(10), || fixture.watcher.dropped() > 0),
        "a {FILES}-file burst into a {CAPACITY}-slot channel with a {MAX_PENDING}-path \
         buffer must reach the drop path; dropped={} pending={}",
        fixture.watcher.dropped(),
        fixture.watcher.pending()
    );

    let dropped = fixture.watcher.dropped();
    let held = fixture.watcher.pending() + fixture.stream.queued();
    eprintln!(
        "stalled consumer: files={FILES} capacity={CAPACITY} max_pending={MAX_PENDING} \
         raw_accepted={accepted} published={published} dropped={dropped} held_now={held} \
         rss_baseline_kib={baseline:?} rss_peak_kib={peak:?}",
        accepted = fixture.watcher.accepted(),
        published = fixture.watcher.published(),
    );

    assert!(
        held <= CAPACITY + MAX_PENDING,
        "the pipeline held {held} events, above the {} it is bounded to",
        CAPACITY + MAX_PENDING
    );
    assert!(
        dropped > 0,
        "coalescing alone cannot absorb {FILES} distinct paths into {MAX_PENDING}; \
         a silent success here would mean the ceiling is not enforced"
    );

    // The loss must be *visible*: a consumer that resumes learns its view has a
    // hole. This is the difference between a drop that is correct and one that is
    // a bug.
    let (events, _) = drain_until_quiet(
        &mut fixture.stream,
        Duration::from_millis(300),
        Duration::from_secs(10),
    );
    assert!(
        overflow_total(&events) > 0,
        "the consumer resumed and was never told it had missed anything: {} events, \
         watcher reports dropped={dropped}",
        events.len()
    );

    match (baseline, peak) {
        (Some(baseline), Some(peak)) => {
            let delta = peak.saturating_sub(baseline);
            assert!(
                delta <= RSS_CEILING_KIB,
                "RSS grew {delta} KiB with a fully stalled consumer, over the \
                 {RSS_CEILING_KIB} KiB ceiling"
            );
            eprintln!("stalled consumer: rss_delta_kib={delta} ceiling_kib={RSS_CEILING_KIB}");
        }
        _ => eprintln!("stalled consumer: rss unavailable, structural bounds still asserted"),
    }
}

#[test]
fn a_deletion_is_reported_as_an_unlink() {
    let mut fixture = Fixture::start(|options| options);
    let file = fixture.root.join("doomed.txt");
    fs::write(&file, b"x").expect("write");
    let (created, _) = drain_until_quiet(
        &mut fixture.stream,
        TEST_DEBOUNCE * 3,
        Duration::from_secs(10),
    );
    assert_eq!(tally(&created).0.get(&file).copied(), Some(ChangeKind::Add));

    fs::remove_file(&file).expect("remove");
    let (removed, quiet) = drain_until_quiet(
        &mut fixture.stream,
        TEST_DEBOUNCE * 3,
        Duration::from_secs(10),
    );
    assert!(quiet, "the stream never went quiet after the deletion");
    assert_eq!(
        tally(&removed).0.get(&file).copied(),
        Some(ChangeKind::Unlink),
        "got {removed:?}"
    );
}

#[test]
fn a_create_then_delete_within_one_window_collapses_to_a_single_unlink() {
    let mut fixture = Fixture::start(|options| options);
    let file = fixture.root.join("transient.txt");
    fs::write(&file, b"x").expect("write");
    fs::remove_file(&file).expect("remove");

    let (events, quiet) = drain_until_quiet(
        &mut fixture.stream,
        TEST_DEBOUNCE * 3,
        Duration::from_secs(10),
    );
    assert!(quiet, "the stream never went quiet");
    let (kinds, counts) = tally(&events);
    assert_eq!(
        counts.get(&file).copied(),
        Some(1),
        "a file that came and went inside one window is one event: {events:?}"
    );
    assert_eq!(kinds.get(&file).copied(), Some(ChangeKind::Unlink));
}

#[test]
fn built_in_ignored_folders_are_never_reported() {
    let mut fixture = Fixture::start(|options| options);
    let ignored = fixture.root.join("node_modules/pkg");
    let reported = fixture.root.join("src/main.rs");
    fs::create_dir_all(&ignored).expect("mkdir");
    fs::create_dir_all(reported.parent().expect("parent")).expect("mkdir");
    fs::write(ignored.join("index.js"), b"x").expect("write");

    // A file written into a *just-created* subdirectory can be missed: inotify
    // watches one directory at a time, so `notify` only adds a watch for `src`
    // once it has processed `src`'s own creation event, and anything written in
    // the gap is never reported by the kernel. Real consumers hit this too. The
    // deterministic answer is to retry the write until the watch is live rather
    // than to sleep and hope — measured during todo 50, recorded in the notepad.
    let seen = poll_until(Duration::from_secs(10), || {
        fs::write(&reported, b"x").expect("write");
        let (events, _) = drain_until_quiet(
            &mut fixture.stream,
            TEST_DEBOUNCE * 2,
            Duration::from_secs(3),
        );
        let paths: BTreeSet<_> = tally(&events).0.into_keys().collect();
        assert!(
            !paths.iter().any(|path| path.starts_with(&ignored)),
            "something under node_modules was reported: {paths:?}"
        );
        paths.contains(&reported)
    });
    assert!(seen, "the source file was never reported");
}

#[test]
fn watcher_ignore_patterns_suppress_matching_paths() {
    let mut fixture = Fixture::start(|options| options.extra_ignore(["**/*.generated.ts"]));
    let suppressed = fixture.root.join("a.generated.ts");
    let reported = fixture.root.join("a.ts");
    fs::write(&suppressed, b"x").expect("write");
    fs::write(&reported, b"x").expect("write");

    let (events, quiet) = drain_until_quiet(
        &mut fixture.stream,
        TEST_DEBOUNCE * 3,
        Duration::from_secs(10),
    );
    assert!(quiet, "the stream never went quiet");
    let paths: BTreeSet<_> = tally(&events).0.into_keys().collect();
    assert!(paths.contains(&reported), "got {paths:?}");
    assert!(!paths.contains(&suppressed), "got {paths:?}");
}

#[test]
fn gitignored_paths_are_suppressed_when_gitignore_is_enabled() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonical root");
    // `require_git` is on by default, and it is satisfied by the *presence* of a
    // `.git` — the same rule `ignore` applies. Without this the `.gitignore`
    // below is silently not applied and the test passes vacuously; that trap cost
    // real time in todo 41 and is recorded in the notepad.
    fs::create_dir(root.join(".git")).expect("fake .git");
    fs::write(root.join(".gitignore"), b"secret.txt\n!keep/secret.txt\n").expect("gitignore");
    fs::create_dir(root.join("keep")).expect("mkdir");

    let options = WatchOptions::new(&root)
        .env(oc_paths::Env::empty().with("ZUNO_EXPERIMENTAL_FILEWATCHER", "true"))
        .debounce(TEST_DEBOUNCE)
        .max_wait(Duration::from_secs(30))
        .gitignore(true);
    let (watcher, mut stream) = Watcher::start(options).expect("watch starts");

    let suppressed = root.join("secret.txt");
    let whitelisted = root.join("keep/secret.txt");
    let reported = root.join("public.txt");
    assert!(
        poll_until(Duration::from_secs(5), || {
            fs::write(&reported, b"x").expect("write");
            watcher.accepted() > 0
        }),
        "inotify never reported a write"
    );
    fs::write(&suppressed, b"x").expect("write");
    fs::write(&whitelisted, b"x").expect("write");

    let (events, quiet) =
        drain_until_quiet(&mut stream, TEST_DEBOUNCE * 3, Duration::from_secs(10));
    assert!(quiet, "the stream never went quiet");
    let paths: BTreeSet<_> = tally(&events).0.into_keys().collect();
    assert!(paths.contains(&reported), "got {paths:?}");
    assert!(
        !paths.contains(&suppressed),
        "a gitignored file was reported: {paths:?}"
    );
    assert!(
        paths.contains(&whitelisted),
        "a `!`-whitelisted file was suppressed: {paths:?}"
    );
}

#[test]
fn the_disable_flag_yields_a_watcher_that_reports_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonical root");
    let options = WatchOptions::new(&root).env(
        oc_paths::Env::empty()
            .with("ZUNO_EXPERIMENTAL_FILEWATCHER", "true")
            .with("ZUNO_EXPERIMENTAL_DISABLE_FILEWATCHER", "true"),
    );
    let (watcher, mut stream) = Watcher::start(options).expect("a disabled watch is not an error");
    assert_eq!(
        watcher.decision(),
        &Decision::Disabled(DisabledReason::ExplicitlyDisabled),
        "the disable flag must win over the enable flag"
    );

    for index in 0..20 {
        fs::write(root.join(format!("{index}.txt")), b"x").expect("write");
    }
    let (events, _) = drain_until_quiet(
        &mut stream,
        Duration::from_millis(300),
        Duration::from_secs(2),
    );
    assert!(events.is_empty(), "a disabled watcher published {events:?}");
    assert_eq!(watcher.accepted(), 0);
}

#[test]
fn without_the_enable_flag_the_project_directory_is_not_watched() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonical root");
    // No flags at all. The oracle still watches `.git` in this state
    // (`watcher.ts:112`), which is why the decision is `VcsOnly` rather than
    // disabled — but with no `vcs_dir` configured there is nothing to watch.
    let options = WatchOptions::new(&root).env(oc_paths::Env::empty());
    let (watcher, mut stream) = Watcher::start(options).expect("watch starts");
    assert_eq!(watcher.decision(), &Decision::VcsOnly);

    for index in 0..20 {
        fs::write(root.join(format!("{index}.txt")), b"x").expect("write");
    }
    let (events, _) = drain_until_quiet(
        &mut stream,
        Duration::from_millis(300),
        Duration::from_secs(2),
    );
    assert!(
        events.is_empty(),
        "the project directory was watched without the enable flag: {events:?}"
    );
}

#[test]
fn only_head_is_reported_from_the_vcs_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonical root");
    let vcs = root.join(".git");
    fs::create_dir(&vcs).expect("mkdir .git");
    fs::write(vcs.join("HEAD"), b"ref: refs/heads/main\n").expect("HEAD");

    // No enable flag: this is the default state, in which the oracle watches
    // `.git` and nothing else.
    let options = WatchOptions::new(&root)
        .env(oc_paths::Env::empty())
        .vcs_dir(&vcs)
        .debounce(TEST_DEBOUNCE)
        .max_wait(Duration::from_secs(30));
    let (watcher, mut stream) = Watcher::start(options).expect("watch starts");
    assert!(watcher.decision().watches_vcs());
    assert!(!watcher.decision().watches_project());

    assert!(
        poll_until(Duration::from_secs(5), || {
            fs::write(vcs.join("HEAD"), b"ref: refs/heads/other\n").expect("HEAD write");
            watcher.accepted() > 0
        }),
        "inotify never reported the HEAD write"
    );
    fs::write(vcs.join("index"), b"noise").expect("index write");

    let (events, quiet) =
        drain_until_quiet(&mut stream, TEST_DEBOUNCE * 3, Duration::from_secs(10));
    assert!(quiet, "the stream never went quiet");
    let paths: BTreeSet<_> = tally(&events).0.into_keys().collect();
    assert!(
        paths.contains(&vcs.join("HEAD")),
        "HEAD was not reported: {paths:?}"
    );
    assert!(
        !paths.contains(&vcs.join("index")),
        "a non-HEAD entry in .git was reported: {paths:?}"
    );
}

#[test]
fn dropping_the_watcher_closes_the_stream() {
    let fixture = Fixture::start(|options| options);
    let Fixture {
        _dir,
        root,
        watcher,
        mut stream,
    } = fixture;
    fs::write(root.join("last.txt"), b"x").expect("write");
    drop(watcher);
    // Draining to `None` proves the sender was dropped, i.e. the flush thread was
    // joined rather than left running.
    assert!(
        poll_until(Duration::from_secs(5), || {
            while stream.try_recv().is_some() {}
            stream.blocking_recv().is_none()
        }),
        "the stream stayed open after the watcher was dropped"
    );
    drop(_dir);
}

#[test]
fn a_path_the_filter_rejects_is_still_answerable_by_the_consumer() {
    // The filter is shared so a consumer that sees a `.gitignore` change can
    // invalidate the cache rather than restart the watch; this pins that surface.
    let fixture = Fixture::start(|options| options.gitignore(true).require_git(false));
    let filter = fixture.watcher.filter();
    assert_eq!(filter.root(), fixture.root.as_path());
    assert!(filter.is_ignored(&fixture.root.join("node_modules/x"), false));
    filter.invalidate();
    assert!(oc_watch::Filter::is_gitignore(Path::new(".gitignore")));
}
