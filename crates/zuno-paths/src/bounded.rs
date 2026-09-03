//! Running a child process under a time ceiling.
//!
//! This crate asks `git` where things are: [`crate::project`] for the worktree root,
//! the git directory and the project id, [`crate::exclude`] for the repository-private
//! exclude file. Every one of those is a small local metadata read, and every one of
//! them is reached from a synchronous function that runs during startup.
//!
//! # Why `Command::output` is not usable here
//!
//! A subprocess reading a `.git` on an unresponsive network mount does not fail — it
//! waits, in a kernel call with nothing to time it out — and so does a `git` that
//! started a credential helper which is itself waiting. [`std::process::Command::output`]
//! waits for that with no ceiling at all, so a directory Zuno cannot reach becomes a
//! process that never gets any further rather than a question it could not answer.
//!
//! The obvious replacement is worse than what it replaces: spawn with piped output,
//! poll [`Child::try_wait`] until the child exits, then read the pipes — and a child
//! that fills a pipe buffer blocks writing while this thread waits for an exit that
//! can no longer come, turning a fast success into a deadlock and then into a
//! timeout. Both pipes are therefore drained by threads that own them, over one
//! [`mpsc`] channel, *before* anything waits for an exit, while this thread keeps the
//! [`Child`], because only whoever holds the `Child` can kill it. One thread per pipe
//! and not one for both: draining stdout to end of file and stderr afterwards
//! deadlocks on a child that fills stderr first, which is the same defect one level
//! down.
//!
//! # What this does not give a caller
//!
//! A ceiling makes a wait finite. It does not make it interruptible, and it does not
//! move it off whatever thread asked: [`output`] blocks its caller for up to `ceiling`,
//! and on a current-thread async runtime that is the whole reactor — no timer, no
//! signal handler, no peer input runs during it. Callers that need cancellation have
//! to move the call to a blocking pool at the async boundary; that is a property of
//! the call site, not something this module can supply.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// The ceiling on one `git` invocation, shared by every git query this crate makes.
///
/// One number for all of them because they are one kind of call: a small local
/// metadata read — `rev-parse`, `remote get-url`, `rev-list --max-parents=0` — whose
/// only way to be slow is the filesystem underneath `.git`. A `.git` on an
/// unresponsive network mount leaves `git` blocked in the kernel with nothing to time
/// it out, and every entry point that reaches one of these is synchronous:
/// `resolve_project`, `worktree_root` and [`crate::exclude::resolve_exclude_path`] all
/// run during startup, including from inside a current-thread tokio runtime, where an
/// unbounded wait is not one slow task but a wedged process.
///
/// Ten seconds, because the ceiling has to clear the slowest *healthy* call rather
/// than the typical one. `rev-list --max-parents=0 HEAD` walks the whole history,
/// which on a million-commit repository without a commit-graph is measured in
/// seconds, and a first `rev-parse` against a live-but-slow mount costs far more than
/// the milliseconds it costs locally. Cutting a healthy repository off is worse than
/// waiting for it: in `project` it would not report an error at all but quietly
/// resolve a different project id, and with it a different snapshot store and a
/// different session bucket, and in `exclude` it would leave the generated state
/// visible in `git status`, which is how an agent ends up reading its own scratch
/// output as the user's uncommitted work. It is not larger because this wait cannot
/// be cancelled and `discover_repository` makes three calls, so ten seconds already
/// admits half a minute of unresponsive startup against a dead mount; past that a
/// user cannot tell a bounded wait from the hang it replaces.
pub(crate) const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a child is re-checked while this thread is waiting for it to exit.
///
/// End of file on the pipes is not the same as having exited, so the status is polled
/// under the same deadline rather than blocked on in [`Child::wait`], which would be
/// unbounded again. A millisecond is far below the cost of the spawn itself, so the
/// poll cannot show up in the healthy path.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// How long a killed child is given to be reaped before it is abandoned.
///
/// `SIGKILL` does not land promptly on a process blocked in an uninterruptible read
/// on a dead mount — exactly the case this ceiling exists for — so the reap is polled
/// briefly and then given up on. An abandoned child costs one process entry until
/// Zuno exits; blocking in `wait` to be tidy would hand the hang straight back.
const KILL_REAP_GRACE: Duration = Duration::from_millis(100);

/// What a child that ran to completion inside its ceiling produced.
///
/// Bytes rather than text because nothing here may reshape the output: the caller
/// decides what a non-UTF-8 answer means, and `resolve_git_path` strips a trailing
/// newline and nothing else, since a path may legally end in a space.
pub(crate) struct Output {
    /// How the child exited. Never a signal-killed status from this module's own
    /// kill, since that path returns [`Failure`] instead.
    pub(crate) status: ExitStatus,
    /// Everything the child wrote to stdout, drained to end of file.
    pub(crate) stdout: Vec<u8>,
    /// Everything the child wrote to stderr, drained to end of file.
    pub(crate) stderr: Vec<u8>,
}

/// Why a child produced nothing a caller could classify.
///
/// Three cases and not one, because they are three different things to tell a user:
/// a machine without the program, a program that would not stop, and a program whose
/// output this process could not collect. A caller that folds them back together is
/// free to; a caller that needs to distinguish a broken mount from a missing binary
/// cannot get that back out of a rendered message.
pub(crate) enum Failure {
    /// The process could not be started: the program is not on `PATH`, or the
    /// working directory no longer exists.
    Spawn(io::Error),
    /// The child ran, but no usable answer came back from it: a pipe that could not
    /// be drained, a reader thread that died, or an exit status that could not be
    /// read. Whatever arrived is discarded rather than reported as a partial answer.
    Lost,
    /// The child was still running when `ceiling` ran out, and was killed.
    TimedOut,
}

/// Run `command` under `ceiling`, draining both of its pipes before waiting for it to
/// exit, and killing it if it outstays the ceiling.
///
/// Stdin is closed. A child of this process must never consume bytes a caller of Zuno
/// is holding for something else, and a `git` that decides to prompt would otherwise
/// wait on a terminal for the whole ceiling.
///
/// Whatever the caller set on `command` is kept; the three standard streams are set
/// here and overwrite anything the caller chose for them.
pub(crate) fn output(command: &mut Command, ceiling: Duration) -> Result<Output, Failure> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Failure::Spawn)?;
    let deadline = Instant::now() + ceiling;

    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        // Unreachable while both are piped above, and still not a reason to leave a
        // child running.
        abandon(&mut child);
        return Err(Failure::Lost);
    };
    let (sender, receiver) = mpsc::channel();
    drain(Stream::Stdout, out, sender.clone());
    drain(Stream::Stderr, err, sender);

    let mut stdout = None;
    let mut stderr = None;
    // Both pipes, before any wait: the whole point of the reader threads is that
    // nothing here blocks on an exit while a child is blocked on a full pipe.
    for _ in 0..2 {
        let received = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        match received {
            Ok((Stream::Stdout, Some(bytes))) => stdout = Some(bytes),
            Ok((Stream::Stderr, Some(bytes))) => stderr = Some(bytes),
            // A failed read, or a reader that panicked and hung up without sending.
            Ok((_, None)) | Err(RecvTimeoutError::Disconnected) => {
                abandon(&mut child);
                return Err(Failure::Lost);
            }
            Err(RecvTimeoutError::Timeout) => {
                abandon(&mut child);
                return Err(Failure::TimedOut);
            }
        }
    }
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        // One thread sends once, so two messages are always one of each; this is the
        // arm that says so rather than an `expect` that could fire on a refactor.
        abandon(&mut child);
        return Err(Failure::Lost);
    };

    // Closing the pipes is not exiting — a child may do the first and keep running —
    // so the status is collected under the same deadline as the output was.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(EXIT_POLL_INTERVAL),
            Ok(None) => {
                abandon(&mut child);
                return Err(Failure::TimedOut);
            }
            Err(_) => {
                abandon(&mut child);
                return Err(Failure::Lost);
            }
        }
    }
}

/// Which of a child's two pipes a reader thread drained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

/// Read `pipe` to end of file on its own thread and report the bytes over `sender`.
///
/// The thread owns the pipe, so it keeps draining whatever the child writes for as
/// long as the child writes it, with nobody waiting on an exit in the meantime.
fn drain<R: Read + Send + 'static>(
    stream: Stream,
    mut pipe: R,
    sender: mpsc::Sender<(Stream, Option<Vec<u8>>)>,
) {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let outcome = pipe.read_to_end(&mut bytes).ok().map(|_| bytes);
        // The receiver is gone whenever this thread lost the race with the ceiling.
        // There is nobody left to tell, and the pipe closes as the thread ends.
        let _ = sender.send((stream, outcome));
    });
}

/// Kill `child`, and reap it if it dies within [`KILL_REAP_GRACE`].
///
/// The reader threads are deliberately not joined. Each is blocked in `read_to_end`
/// until its pipe closes, so joining would restore the unbounded wait in the one case
/// that matters — a child the kernel will not let die. Killing closes the write ends,
/// so the threads finish on their own wherever the child can die at all.
///
/// The kill reaches the program that was spawned and not anything it started. That is
/// sound for the calls this crate makes, which are local git reads that spawn no
/// helper; it would not be sound for a fetch.
fn abandon(child: &mut Child) {
    let _ = child.kill();
    let deadline = Instant::now() + KILL_REAP_GRACE;
    loop {
        match child.try_wait() {
            Ok(None) if Instant::now() < deadline => std::thread::sleep(EXIT_POLL_INTERVAL),
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four times the usual 64 KiB pipe capacity, so a child cannot finish writing
    /// one pipe unless somebody is draining it while this process waits.
    const LARGE: usize = 256 * 1024;

    /// Enough of a ceiling that a healthy `sh` on a loaded machine is never the thing
    /// that ends a case, and short enough that a lost bound fails a test rather than
    /// hanging the suite.
    const GENEROUS: Duration = Duration::from_secs(30);

    #[cfg(unix)]
    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    /// The mechanism: a child that never exits is killed at the ceiling instead of
    /// waited on for as long as it feels like running.
    #[test]
    #[cfg(unix)]
    fn a_child_that_never_exits_is_killed_at_the_ceiling() {
        let ceiling = Duration::from_millis(250);
        let started = Instant::now();

        let outcome = output(&mut sh("exec sleep 120"), ceiling);

        let elapsed = started.elapsed();
        assert!(
            matches!(outcome, Err(Failure::TimedOut)),
            "a child that outstays its ceiling is a timeout and not any other failure"
        );
        assert!(
            elapsed >= ceiling,
            "returned in {elapsed:?}, inside the ceiling"
        );
        assert!(
            elapsed < GENEROUS,
            "waited {elapsed:?} on a child sleeping two minutes"
        );
    }

    /// The reason there are two reader threads. This child writes a quarter of a
    /// megabyte to *both* pipes and only then exits, so an implementation that
    /// drained one pipe to end of file before touching the other — or that waited for
    /// the exit first — would deadlock here and report a healthy call as a timeout.
    #[test]
    #[cfg(unix)]
    fn a_child_that_fills_both_pipes_is_not_a_deadlock() {
        let started = Instant::now();

        let outcome = output(
            &mut sh(&format!(
                "yes | head -c {LARGE} & yes | head -c {LARGE} >&2; wait"
            )),
            GENEROUS,
        );

        let Ok(collected) = outcome else {
            panic!("a child that fills both pipes must still be collected in full");
        };
        assert!(collected.status.success());
        assert_eq!(collected.stdout.len(), LARGE, "stdout was truncated");
        assert_eq!(collected.stderr.len(), LARGE, "stderr was truncated");
        assert!(
            started.elapsed() < GENEROUS,
            "a healthy call must not be decided by the ceiling"
        );
    }

    /// Everything a caller classifies on comes back: the exit code it objected with,
    /// and the bytes it objected with, kept apart.
    #[test]
    #[cfg(unix)]
    fn the_exit_status_and_both_streams_reach_the_caller() {
        let collected = output(
            &mut sh("printf 'to out'; printf 'to err' >&2; exit 3"),
            GENEROUS,
        )
        .unwrap_or_else(|_| panic!("a non-zero exit is an answer, not a failure"));

        assert_eq!(collected.status.code(), Some(3));
        assert_eq!(collected.stdout, b"to out");
        assert_eq!(collected.stderr, b"to err");
    }

    /// Stdin is closed rather than inherited: a child must never consume the bytes a
    /// caller of Zuno is holding for something else.
    #[test]
    #[cfg(unix)]
    fn stdin_is_closed_so_a_child_reads_nothing_and_waits_for_nothing() {
        let started = Instant::now();

        let collected = output(&mut sh("cat"), GENEROUS).unwrap_or_else(|_| {
            panic!("a child reading a closed stdin sees end of file immediately")
        });

        assert_eq!(collected.stdout, b"");
        assert!(
            started.elapsed() < GENEROUS,
            "a `cat` on a closed stdin must not be decided by the ceiling"
        );
    }

    /// A program that is not there is a spawn failure and not a timeout, so a caller
    /// can tell a machine without git from a mount it cannot reach.
    #[test]
    fn a_program_that_cannot_be_started_is_a_spawn_failure() {
        let mut command = Command::new("zuno-paths-no-such-program");

        let outcome = output(&mut command, GENEROUS);

        assert!(
            matches!(outcome, Err(Failure::Spawn(_))),
            "an absent program is a spawn failure"
        );
    }
}
