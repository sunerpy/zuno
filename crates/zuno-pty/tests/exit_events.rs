//! The exit event's visibility contract, asserted under real contention.
//!
//! `zuno-pty` has now produced two flakes from one root pattern: a state change
//! becoming observable *before* the event that reports it. The first was
//! `PtyOutput::Ended` arriving before the reader had drained the pty, so a
//! subscriber that stopped at `Ended` lost the tail of the output. The second is
//! this one: `PtyEvent::Exited` never arriving at all.
//!
//! Both are invisible to a serial test run, which is why this file exists and why
//! it hammers rather than samples.

mod common;

use common::{spawn_script, wait_for_exit};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use zuno_pty::{PtyEvent, PtyService};

/// Workers × iterations. 1,280 total.
///
/// Chosen from measurement, not taste: the pre-fix loss rate on this host was
/// 7/1,280, so a run an order of magnitude smaller would have reported a clean
/// pass against the bug this test exists to catch. The whole file costs ~1.5s,
/// which is the budget that bought the sensitivity — do not trade it away.
const WORKERS: usize = 32;
const ITERATIONS: usize = 40;

/// Exits observed in the drain that follows the status read, as required.
static PROMPT: AtomicUsize = AtomicUsize::new(0);
/// Exits that showed up only after further polling: a timing regression.
static LATE: AtomicUsize = AtomicUsize::new(0);
/// Exits that never showed up: a correctness regression.
static LOST: AtomicUsize = AtomicUsize::new(0);
/// `Deleted` seen ahead of `Exited` for the same session: an ordering regression.
static DELETED_FIRST: AtomicUsize = AtomicUsize::new(0);

/// How long to keep looking for a straggler before calling it lost.
///
/// Only reached when the contract is already broken, so it costs nothing on a
/// healthy tree; it exists purely to tell *late* apart from *lost*, which is the
/// difference between a timing bug and a dropped event.
const STRAGGLER_GRACE: Duration = Duration::from_secs(3);

/// One iteration of the sequence a consumer actually performs:
/// read status, act on it, then read the event stream.
fn observe_one_exit() {
    let service = PtyService::new(std::env::temp_dir());
    let mut events = service.subscribe();
    let id = spawn_script(&service, "exit 5").id;

    // The contract under test: once this returns, `PtyEvent::Exited` is already in
    // the channel. Everything after this line is a plain drain — it must not need
    // to wait for anything.
    wait_for_exit(&service, &id);
    service.remove(&id).expect("the exited session is retained");

    let mut exited = None;
    let mut order: Vec<&str> = Vec::new();
    while let Ok(event) = events.try_recv() {
        match &event {
            PtyEvent::Created { info } if info.id == id => order.push("created"),
            PtyEvent::Exited {
                id: other,
                exit_code,
            } if *other == id => {
                exited = Some(*exit_code);
                order.push("exited");
            }
            PtyEvent::Deleted { id: other } if *other == id => order.push("deleted"),
            _ => {}
        }
    }

    if let (Some(deleted), Some(exit)) = (
        order.iter().position(|entry| *entry == "deleted"),
        order.iter().position(|entry| *entry == "exited"),
    ) && deleted < exit
    {
        DELETED_FIRST.fetch_add(1, Ordering::Relaxed);
    }

    if exited == Some(Some(5)) {
        PROMPT.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let deadline = Instant::now() + STRAGGLER_GRACE;
    while Instant::now() < deadline {
        match events.try_recv() {
            Ok(PtyEvent::Exited { id: other, .. }) if other == id => {
                LATE.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_micros(200)),
        }
    }
    LOST.fetch_add(1, Ordering::Relaxed);
}

/// A removal racing a natural exit must never cost the exit event.
///
/// The bug this guards: the exit was announced by the registry behind a
/// `sessions.contains_key(id)` early return, while the session's status flipped to
/// `Exited` earlier, under a different lock. A `remove` landing in that window made
/// the announcement skip itself, so a consumer that had *already seen* `Exited`
/// through `get`/`list` could never learn the exit code. Measured on this host:
/// **7 of 1,280 permanently lost** before the fix, 0 after.
///
/// `announce` therefore runs inside the session state lock — the same lock every
/// status reader takes — which is what makes "status implies event" a happens-before
/// rather than a hope.
#[test]
fn an_exit_event_is_never_lost_when_a_removal_races_the_exit() {
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..ITERATIONS {
                    observe_one_exit();
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker thread");
    }

    let total = WORKERS * ITERATIONS;
    let prompt = PROMPT.load(Ordering::Relaxed);
    let late = LATE.load(Ordering::Relaxed);
    let lost = LOST.load(Ordering::Relaxed);
    let deleted_first = DELETED_FIRST.load(Ordering::Relaxed);

    // Reported separately on purpose: `lost` is a dropped event, `late` is an
    // ordering slip, and conflating them would send the next reader hunting the
    // wrong bug.
    assert_eq!(
        lost, 0,
        "{lost} of {total} exit events were never delivered \
         (prompt={prompt} late={late}); the event must reach the channel before \
         the status becomes observable"
    );
    assert_eq!(
        late, 0,
        "{late} of {total} exit events arrived only after extra polling \
         (prompt={prompt}); a consumer that reads status then drains must not have to wait"
    );
    assert_eq!(
        deleted_first, 0,
        "{deleted_first} of {total} sessions reported Deleted before Exited"
    );
    assert_eq!(prompt, total, "every iteration must observe its own exit");
}
