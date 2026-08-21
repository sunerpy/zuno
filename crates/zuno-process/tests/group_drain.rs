//! The contained group must be unable to run before the guard reports it gone.
//!
//! The TUI hands the terminal back as soon as the editor supervisor — its own direct
//! child — has been reaped (`zuno-tui/src/views/external.rs::terminate_and_reap`).
//! Every process below that supervisor is invisible from there, so the supervisor's
//! exit is only a sound signal if the guard refuses to exit while any member of the
//! contained group could still execute another instruction. These tests take the
//! observation at exactly the boundary production uses: a blocking wait on the
//! supervisor, then one read of the descendant's `/proc` state.
//!
//! `Z` is accepted and a live state is not. A zombie has no address space and no file
//! descriptors, so it cannot emit a byte at the terminal; a sleeping or runnable
//! process can, the moment it is scheduled. The last transition — zombie to reaped —
//! belongs to whoever inherits the orphan, which is neither ours to drive nor needed
//! for the terminal to be safe.
//!
//! The read is sound in one shot because the states are monotone across this
//! boundary: after the guard has exited a live member can only die and a zombie can
//! only be reaped. Nothing can move back towards being able to run, so a single
//! sample cannot report safety that was not there.
//!
//! # These assertions are strict, and an earlier tolerant version was wrong
//!
//! The wait is bounded, so the guard may in principle give up and say so on its stderr.
//! An earlier version of these tests accepted that as a pass, on the reasoning that a
//! reported hand-back is a different thing from a silent one. That was a mistake, for a
//! measurable reason: across every run of the finished drain — two hundred at a load
//! average of 128, sixty at 134, two hundred at 91 — the give-up branch was taken
//! **zero** times. The tolerance therefore protected nothing that happens, while
//! silently admitting a drain that gives up on every call. Anything that makes the
//! give-up branch reachable in these fixtures is a regression in the drain, so it must
//! fail here rather than be waved through by a log line.
//!
//! What these tests can and cannot see is worth stating, because it is not obvious.
//! They detect the absence of a settle step entirely. They do **not** detect a settle
//! step degraded to a single observation: one `/proc` walk is itself enough delay for a
//! plain `SIGKILL`ed descendant on this host, measured at zero failures in sixty runs
//! at load 134. Convergence — re-signalling so a member that joined the group during
//! the kernel's walk is still reached — is provable only against a group nobody else
//! signals, which is a unit test, not this file. See
//! `a_group_nobody_else_will_kill_is_driven_quiet_by_the_wait_itself`.

#![cfg(target_os = "linux")]

use std::io::Read as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Generous: these spawn a four-deep process tree, and the suite runs 32-wide.
const TIMEOUT: Duration = Duration::from_secs(20);

/// A payload that backgrounds a descendant and then blocks until told to stop.
///
/// `sleep 3600 &` reproduces the shape that matters: the descendant's parent is the
/// payload, so when the payload dies the descendant is reparented away and can no
/// longer be reached by any `wait` the guard is able to perform.
const BACKGROUNDS_A_DESCENDANT: &str = "sleep 3600 & printf '%s' \"$!\" > \"$1\"; \
     printf ready > \"$2\"; read line";

/// The same, but the descendant ignores `SIGTERM` and only `SIGKILL` can stop it.
const BACKGROUNDS_A_STUBBORN_DESCENDANT: &str = "/bin/sh -c 'trap \"\" TERM; while :; do sleep 1; done' & printf '%s' \"$!\" > \"$1\"; \
     printf ready > \"$2\"; read line";

#[test]
fn a_cancelled_guarded_editor_leaves_no_descendant_able_to_run() {
    let mut tree = GuardedTree::spawn(BACKGROUNDS_A_DESCENDANT);
    let descendant = tree.descendant;

    let teardown = tree.signal_and_observe(descendant);

    teardown.assert_drained(descendant, "a cancelled editor");
}

#[test]
fn a_guarded_editor_that_exits_by_itself_leaves_no_descendant_able_to_run() {
    let mut tree = GuardedTree::spawn(BACKGROUNDS_A_DESCENDANT);
    let descendant = tree.descendant;

    let teardown = tree.release_and_observe(descendant);

    teardown.assert_drained(descendant, "an editor that exited by itself");
}

#[test]
fn a_descendant_that_ignores_termination_still_ends_the_guard_within_its_budget() {
    let mut tree = GuardedTree::spawn(BACKGROUNDS_A_STUBBORN_DESCENDANT);
    let descendant = tree.descendant;

    let started = Instant::now();
    let teardown = tree.signal_and_observe(descendant);
    let waited = started.elapsed();

    teardown.assert_drained(descendant, "a descendant that ignores SIGTERM");
    assert!(
        waited < TIMEOUT,
        "the guard waited {waited:?} on a descendant that ignores SIGTERM, which is a \
         hang rather than a bounded wait"
    );
}

struct Teardown {
    state: Option<char>,
    reported: String,
}

impl Teardown {
    fn assert_drained(&self, descendant: u32, editor: &str) {
        assert!(
            matches!(self.state, None | Some('Z')),
            "after {editor} the guard exited while descendant {descendant} was in state \
             {:?}, so the terminal goes back to the TUI while a process that can still \
             write to it is alive; the guard's stderr held: {:?}",
            self.state,
            self.reported
        );
        assert!(
            !self.reported.contains("still had"),
            "after {editor} the guard gave up on the group instead of draining it. The \
             descendant happened to die anyway, so the state read is clean, but a drain \
             that reaches its deadline here reaches it on every call: {:?}",
            self.reported
        );
    }
}

/// One guarded payload, its supervisor, and the descendant it left behind.
struct GuardedTree {
    supervisor: Child,
    descendant: u32,
    _directory: tempfile::TempDir,
}

impl GuardedTree {
    fn spawn(payload: &str) -> Self {
        let directory = tempfile::tempdir().expect("temporary fixture directory");
        let descendant_path = directory.path().join("descendant.pid");
        let ready_path = directory.path().join("ready");
        let mut supervisor = Command::new(env!("CARGO_BIN_EXE_zuno-process-fixture"))
            .args([
                zuno_process::GUARD_MARKER,
                "supervise",
                &std::process::id().to_string(),
                "--",
                "/bin/sh",
                "-c",
                payload,
                "sh",
            ])
            .arg(&descendant_path)
            .arg(&ready_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the guarded payload");
        let descendant = wait_for_descendant(&mut supervisor, &ready_path, &descendant_path);
        Self {
            supervisor,
            descendant,
            _directory: directory,
        }
    }

    /// Cancels the way the TUI does: signal the supervisor, then wait for it.
    fn signal_and_observe(&mut self, descendant: u32) -> Teardown {
        let supervisor = rustix::process::Pid::from_raw(self.supervisor.id() as i32)
            .expect("the supervisor has a non-zero PID");
        rustix::process::kill_process(supervisor, rustix::process::Signal::TERM)
            .expect("signal the editor supervisor");
        self.wait_and_observe(descendant)
    }

    /// Lets the payload finish on its own by closing the stdin it is reading.
    fn release_and_observe(&mut self, descendant: u32) -> Teardown {
        drop(self.supervisor.stdin.take());
        self.wait_and_observe(descendant)
    }

    fn wait_and_observe(&mut self, descendant: u32) -> Teardown {
        // The pipe must be taken before the wait: the state read has to happen the
        // instant the guard is reaped, with no read syscall in between.
        let mut stderr = self.supervisor.stderr.take().expect("the guard's stderr");
        let status = self.supervisor.wait().expect("wait for the supervisor");
        let state = process_state(descendant);
        assert!(
            status.code().is_some() || status.signal().is_some(),
            "the supervisor neither exited nor was signalled: {status}"
        );
        let mut reported = String::new();
        let _read = stderr.read_to_string(&mut reported);
        Teardown { state, reported }
    }
}

impl Drop for GuardedTree {
    fn drop(&mut self) {
        if self.supervisor.try_wait().ok().flatten().is_none() {
            let _killed = self.supervisor.kill();
            let _reaped = self.supervisor.wait();
        }
        // A descendant that outlived its guard is exactly the hazard under test, so
        // it must not be left running for the rest of the suite either.
        if let Some(descendant) = rustix::process::Pid::from_raw(self.descendant as i32) {
            let _killed = rustix::process::kill_process(descendant, rustix::process::Signal::KILL);
        }
    }
}

fn wait_for_descendant(supervisor: &mut Child, ready: &Path, descendant: &Path) -> u32 {
    let started = Instant::now();
    loop {
        if ready.exists()
            && let Ok(contents) = std::fs::read_to_string(descendant)
            && let Ok(pid) = contents.trim().parse::<u32>()
        {
            return pid;
        }
        if let Some(status) = supervisor.try_wait().expect("poll the supervisor") {
            panic!("the guarded payload exited before backgrounding a descendant: {status}");
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "the guarded payload never backgrounded a descendant"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The process state letter from `/proc/<pid>/stat`, or `None` once it is reaped.
fn process_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` is parenthesised and may itself contain spaces and parentheses, so the
    // fixed fields start after its last `)`.
    let fields = stat.get(stat.rfind(')')? + 1..)?;
    fields.split_whitespace().next()?.chars().next()
}
