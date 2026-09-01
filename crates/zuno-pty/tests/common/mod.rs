//! Deterministic helpers for driving real PTY sessions in a test.
//!
//! Every wait here is a bounded poll with a deadline. A fixed `sleep` as a
//! synchronisation primitive is what makes a PTY suite flake: process startup and
//! kernel pty scheduling vary by an order of magnitude under parallel `cargo test`,
//! so a sleep tuned on an idle machine fails on a loaded one. The rule is todo 50's
//! and it held there for 3/3 identical runs.
//!
//! Each integration test binary compiles this module independently, so a helper
//! used by two of the three is unavoidably dead in the third — hence the blanket
//! allow. The alternative is three divergent copies of the polling rules, which is
//! exactly the duplication that lets one copy drift back to a fixed sleep.
#![allow(
    dead_code,
    reason = "this shared module is compiled separately by tests that use different helper subsets"
)]

use std::time::{Duration, Instant};

use zuno_pty::{PtyId, PtyInfo, PtyService, PtyStatus};

/// Longest any single wait may take before the test fails with what it observed.
///
/// Generous because these tests run six-way concurrently in the flake check, and a
/// deadline that is merely *reached* costs nothing when the condition is already
/// true — only a genuine failure pays it.
pub const BUDGET: Duration = Duration::from_secs(20);

/// Gap between polls. Short enough to keep a satisfied condition near-instant.
const STEP: Duration = Duration::from_millis(5);

/// Polls `condition` until it is true or [`BUDGET`] elapses.
pub fn poll_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + BUDGET;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(STEP);
    }
}

/// Blocks until a session reports [`PtyStatus::Exited`].
///
/// # Panics
///
/// If the session does not exit within [`BUDGET`], or vanished from the service.
pub fn wait_for_exit(service: &PtyService, id: &PtyId) -> PtyInfo {
    let exited = poll_until(|| {
        service
            .get(id)
            .is_ok_and(|info| info.status == PtyStatus::Exited)
    });
    let observed = service.get(id);
    assert!(
        exited,
        "session {id} did not exit within {BUDGET:?}; last observed {observed:?}"
    );
    observed.expect("the poll above proved the session is present")
}

/// Blocks until an exited session is recorded in the retention queue.
///
/// Exit publication and retention recording are deliberately separate phases:
/// the waiter marks the session exited while holding its state lock, then records
/// it after releasing that lock to avoid lock inversion with the registry. Tests
/// that need an exact exit order must wait for both phases before releasing the
/// next process.
///
/// # Panics
///
/// If the session is not retained within [`BUDGET`].
pub fn wait_for_retained_exit(service: &PtyService, id: &PtyId) {
    let retained = poll_until(|| service.retained_exited().contains(id));
    assert!(
        retained,
        "session {id} was not recorded in the retention queue within {BUDGET:?}; \
         retained exits {:?}",
        service.retained_exited()
    );
}

/// Blocks until a session either reports [`PtyStatus::Exited`] or is gone.
///
/// Needed whenever more sessions than the retention cap exit at once: an eviction
/// is only ever triggered from an exit, so a session that has vanished has
/// necessarily exited, and demanding to *see* its `exited` status is a race the
/// implementation is right to win.
///
/// # Panics
///
/// If the session is still running after [`BUDGET`].
pub fn wait_for_exit_or_eviction(service: &PtyService, id: &PtyId) {
    let settled = poll_until(|| match service.get(id) {
        Ok(info) => info.status == PtyStatus::Exited,
        Err(_) => true,
    });
    assert!(
        settled,
        "session {id} neither exited nor was evicted within {BUDGET:?}; last observed {:?}",
        service.get(id)
    );
}

/// Blocks until a session's retained output contains `needle`.
///
/// # Panics
///
/// If it does not appear within [`BUDGET`], reporting what was retained instead.
pub fn wait_for_output(service: &PtyService, id: &PtyId, needle: &str) {
    let found = poll_until(|| {
        service
            .retained_output(id)
            .is_ok_and(|retained| String::from_utf8_lossy(&retained.bytes).contains(needle))
    });
    assert!(
        found,
        "session {id} never produced {needle:?} within {BUDGET:?}; retained {:?}",
        service
            .retained_output(id)
            .map(|retained| String::from_utf8_lossy(&retained.bytes).into_owned())
    );
}

/// Blocks until a session has produced at least `bytes` in total.
///
/// # Panics
///
/// If the total does not reach `bytes` within [`BUDGET`].
pub fn wait_for_total_written(service: &PtyService, id: &PtyId, bytes: u64) {
    let reached = poll_until(|| {
        service
            .retained_output(id)
            .is_ok_and(|retained| retained.total_written >= bytes)
    });
    assert!(
        reached,
        "session {id} produced only {:?} of the required {bytes} bytes within {BUDGET:?}",
        service
            .retained_output(id)
            .map(|retained| retained.total_written)
    );
}

/// A `sh -c` session, which is the smallest thing that can be scripted portably.
///
/// # Panics
///
/// If the pty cannot be opened or `sh` cannot be spawned.
pub fn spawn_script(service: &PtyService, script: &str) -> PtyInfo {
    service
        .create(zuno_pty::CreateInput {
            command: Some("/bin/sh".to_owned()),
            args: Some(vec!["-c".to_owned(), script.to_owned()]),
            ..Default::default()
        })
        .expect("a /bin/sh pty session")
}
