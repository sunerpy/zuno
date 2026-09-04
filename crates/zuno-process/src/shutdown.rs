//! Contained shutdown of one runtime-owned child under ceilings the call site fixes.
//!
//! Every resident host Zuno spawns through tokio ends the same way: request that the child's
//! tree stop, collect the child, and settle the tasks that hold its pipes. Each of those steps
//! can wait forever on a host that misbehaves. The process-control call is a bare `kill(2)` on
//! Unix but a `taskkill /pid N /f /t` tree walk waited on synchronously on Windows; a guard or
//! payload stuck in uninterruptible I/O never becomes reapable; and a pipe stays open for as
//! long as any process the child leaked still holds its write end, so a reader that waits for
//! EOF waits for that process's whole life. Each step here therefore runs under a ceiling the
//! caller passes in, the process-control call runs off the runtime worker, and a reader task
//! whose ceiling expires is aborted rather than dropped, because dropping a `JoinHandle`
//! detaches the task and leaves it holding the pipe.
//!
//! The ceilings are the caller's. This module holds no `Duration` of its own, so no spawn site
//! inherits a bound it did not state, and a bound cannot be widened by anything the child, the
//! model, or configuration supplies unless a caller derives it from those, which it must not.

use std::fmt;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::Child;
use tokio::task::JoinHandle;

/// The ceilings one call site places on one contained shutdown.
///
/// Every field is meant to be a `const` at the call site. None is a default, because a default
/// would be a bound the call site never stated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownCeilings {
    /// Bound on the blocking process-control call.
    ///
    /// On Unix that call is `kill(2)` and returns at once. On Windows it spawns `taskkill /pid N
    /// /f /t` and waits for the tree walk, which is why the call runs off the runtime worker and
    /// why its join is bounded: moving it off the worker does not make it finish.
    pub process_control: Duration,
    /// Bound on collecting the child after the request.
    ///
    /// With a guard active the child is the guard, which stops and reaps the group it owns and
    /// then exits; without one it is the payload itself. Exceeding this ceiling means the child
    /// is still running or unreapable, and the caller must treat the outcome as uncertain.
    pub reap: Duration,
    /// Bound on the reader tasks that hold the child's pipes, shared by all of them.
    ///
    /// One deadline for every reader, not one per reader, so a caller with several pipes pays
    /// the ceiling once. A reader that has not finished by then is aborted, which drops its read
    /// end and releases the pipe.
    pub drain: Duration,
}

/// How the shutdown request reached the child.
#[derive(Debug)]
pub enum ShutdownRequest {
    /// The child had already exited, so nothing was signalled.
    AlreadyExited,
    /// The process-control call returned success.
    Delivered,
    /// The process-control call returned an error.
    Failed(io::Error),
    /// The process-control call did not return within [`ShutdownCeilings::process_control`].
    ///
    /// The call itself cannot be cancelled and may still complete later; only this await was
    /// released.
    TimedOut,
}

impl fmt::Display for ShutdownRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExited => formatter.write_str("the child had already exited"),
            Self::Delivered => formatter.write_str("the shutdown request was delivered"),
            Self::Failed(error) => write!(formatter, "the shutdown request failed: {error}"),
            Self::TimedOut => {
                formatter.write_str("the process-control call did not return within its ceiling")
            }
        }
    }
}

/// Whether the child was collected.
#[derive(Debug)]
pub enum ChildReap {
    /// The child exited and was reaped with this status.
    Reaped(ExitStatus),
    /// The child did not become reapable within [`ShutdownCeilings::reap`].
    ///
    /// Whatever the child was doing is still unknown; the caller must report an uncertain
    /// outcome rather than a clean stop.
    TimedOut,
    /// Waiting for the child failed.
    Failed(io::Error),
}

impl fmt::Display for ChildReap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reaped(status) => write!(formatter, "the child was reaped ({status})"),
            Self::TimedOut => {
                formatter.write_str("the child was not collected within the reap ceiling")
            }
            Self::Failed(error) => write!(formatter, "waiting for the child failed: {error}"),
        }
    }
}

/// What became of the reader tasks holding the child's pipes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReaderDrain {
    /// Readers that finished on their own within the drain ceiling, including ones that had
    /// already panicked or been cancelled.
    pub finished: usize,
    /// Readers that were still running at the drain ceiling and were aborted.
    pub aborted: usize,
}

impl fmt::Display for ReaderDrain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} reader(s) finished, {} aborted at the drain ceiling",
            self.finished, self.aborted
        )
    }
}

/// The record of one contained shutdown.
///
/// Nothing here is a promise: a timed-out step is reported as timed out, never folded into a
/// success, so a caller can only ever add uncertainty to what it reports.
#[derive(Debug)]
pub struct ContainedShutdown<T> {
    /// How the request reached the child.
    pub request: ShutdownRequest,
    /// Whether the child was collected.
    pub reap: ChildReap,
    /// What became of the readers.
    pub readers: ReaderDrain,
    /// Each reader's output in the order the readers were given; `None` for one that was aborted,
    /// panicked, or had already been cancelled.
    pub outputs: Vec<Option<T>>,
}

impl<T> ContainedShutdown<T> {
    /// True when the child was reaped and every reader finished on its own.
    ///
    /// A request that failed or timed out does not prevent this: the child being gone is what
    /// matters, and the request outcome is still available for logging.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        matches!(self.reap, ChildReap::Reaped(_)) && self.readers.aborted == 0
    }

    /// The child's exit status, when it was collected.
    #[must_use]
    pub fn exit_status(&self) -> Option<ExitStatus> {
        match self.reap {
            ChildReap::Reaped(status) => Some(status),
            ChildReap::TimedOut | ChildReap::Failed(_) => None,
        }
    }
}

impl<T> fmt::Display for ContainedShutdown<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}; {}; {}",
            self.request, self.reap, self.readers
        )
    }
}

/// Stops one child's tree, collects the child, and settles its readers, each under its ceiling.
///
/// The request is [`request_contained_process_shutdown`](crate::request_contained_process_shutdown),
/// so the child must have been launched through a guarded launch helper or as its own
/// process-group leader. The sequence is: skip the request for a child that already exited; run
/// the process-control call off the runtime worker under `ceilings.process_control`; wait for the
/// child under `ceilings.reap`; then drain `readers` under one shared `ceilings.drain` deadline,
/// aborting any that are still running. Readers are drained on every path, including one where
/// the child could not be reaped, so no path leaves a task holding a pipe.
pub async fn shutdown_contained_child<T>(
    child: &mut Child,
    readers: Vec<JoinHandle<T>>,
    ceilings: ShutdownCeilings,
) -> ContainedShutdown<T>
where
    T: Send + 'static,
{
    shutdown_contained_child_with(
        child,
        readers,
        ceilings,
        crate::request_contained_process_shutdown,
    )
    .await
}

/// [`shutdown_contained_child`] with the process-control call injected.
///
/// `request` is a parameter so a test can drive this exact dispatch with a call that really does
/// block: on a Unix host the production call is a bare `kill(2)` that returns immediately, so
/// nothing else can show that the dispatch leaves the runtime worker or that its join is bounded.
/// Production callers use [`shutdown_contained_child`].
pub async fn shutdown_contained_child_with<T, F>(
    child: &mut Child,
    readers: Vec<JoinHandle<T>>,
    ceilings: ShutdownCeilings,
    request: F,
) -> ContainedShutdown<T>
where
    T: Send + 'static,
    F: FnOnce(u32) -> io::Result<()> + Send + 'static,
{
    let (request, reap) = match child.try_wait() {
        Ok(Some(status)) => (ShutdownRequest::AlreadyExited, ChildReap::Reaped(status)),
        Ok(None) | Err(_) => {
            let request = match child.id() {
                Some(pid) => {
                    match off_runtime_worker(ceilings.process_control, move || request(pid)).await {
                        Some(Ok(())) => ShutdownRequest::Delivered,
                        Some(Err(error)) => ShutdownRequest::Failed(error),
                        None => ShutdownRequest::TimedOut,
                    }
                }
                None => ShutdownRequest::AlreadyExited,
            };
            let reap = match tokio::time::timeout(ceilings.reap, child.wait()).await {
                Ok(Ok(status)) => ChildReap::Reaped(status),
                Ok(Err(error)) => ChildReap::Failed(error),
                Err(_elapsed) => ChildReap::TimedOut,
            };
            (request, reap)
        }
    };
    let (readers, outputs) = drain_readers(ceilings.drain, readers).await;
    ContainedShutdown {
        request,
        reap,
        readers,
        outputs,
    }
}

/// Runs one blocking process-control call off the runtime worker, waiting at most `ceiling`.
///
/// [`request_contained_process_shutdown`](crate::request_contained_process_shutdown) keeps a
/// blocking signature because synchronous callers need it, and on Unix it is a bare `kill(2)`.
/// On Windows the same call spawns `taskkill /f /t` and waits for the whole tree walk. Every
/// session runtime Zuno builds is current-thread, so an inline call would freeze the provider
/// stream and the client event pump until the walk finished, and a cancellation is exactly when
/// those must keep draining.
///
/// The join is bounded because moving the call off the worker does not make it finish. `None`
/// means the call did not return within `ceiling`, or panicked; either way the caller has no
/// result and must not claim one. Aborting is not attempted: a blocking task that has started
/// cannot be cancelled, so the ceiling releases this await, not the operating-system call, and a
/// call that never returns keeps its blocking-pool thread for the life of the process. That is
/// bounded only by tokio's default of 512 blocking threads per runtime, so a host whose
/// `taskkill` wedges on every cancellation degrades the whole runtime's blocking pool, which is
/// still strictly better than the inline call that froze the runtime on the first occurrence.
pub async fn off_runtime_worker<T>(
    ceiling: Duration,
    operation: impl FnOnce() -> T + Send + 'static,
) -> Option<T>
where
    T: Send + 'static,
{
    tokio::time::timeout(ceiling, tokio::task::spawn_blocking(operation))
        .await
        .ok()
        .and_then(Result::ok)
}

/// Settles reader tasks under one shared deadline, aborting any still running when it passes.
///
/// A reader that owns a pipe read end returns only at EOF, and EOF needs every writer to have
/// closed the pipe. A process the child leaked outside its own group survives the group kill and
/// keeps the pipe open, so a reader that is merely dropped keeps running, keeps the read end and
/// its buffer alive, and keeps its slot in the runtime for as long as that process lives.
/// Aborting releases all three. Nothing is lost that was not already lost: a reader that has not
/// finished by the deadline never had an output to report, and its slot is `None`.
///
/// The deadline is one `ceiling` from entry for all readers together, so the caller pays the
/// bound once regardless of how many pipes the child had.
pub async fn drain_readers<T>(
    ceiling: Duration,
    readers: Vec<JoinHandle<T>>,
) -> (ReaderDrain, Vec<Option<T>>) {
    let deadline = tokio::time::Instant::now() + ceiling;
    let mut drain = ReaderDrain::default();
    let mut outputs = Vec::with_capacity(readers.len());
    for mut reader in readers {
        match tokio::time::timeout_at(deadline, &mut reader).await {
            Ok(joined) => {
                drain.finished += 1;
                outputs.push(joined.ok());
            }
            Err(_elapsed) => {
                reader.abort();
                drain.aborted += 1;
                outputs.push(None);
            }
        }
    }
    (drain, outputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Instant;
    use tokio::io::AsyncReadExt as _;

    /// Ceilings small enough to keep each test short, large enough that scheduling jitter on a
    /// loaded host cannot fire them early.
    const CEILINGS: ShutdownCeilings = ShutdownCeilings {
        process_control: Duration::from_millis(400),
        reap: Duration::from_millis(400),
        drain: Duration::from_millis(400),
    };
    /// Slack over a ceiling, for process spawn and scheduling on a loaded machine.
    const SLACK: Duration = Duration::from_secs(5);
    /// Long enough that a starved current-thread runtime is unmistakable, shorter than the
    /// process-control ceiling so the call itself is what ends.
    const BLOCKING_CALL: Duration = Duration::from_millis(250);
    /// Longer than every ceiling under test, so the ceiling is what ends the wait.
    const WEDGED_CALL: Duration = Duration::from_secs(20);
    const TICK: Duration = Duration::from_millis(5);
    /// How long a stand-in child stays alive: longer than every ceiling plus slack, so a test can
    /// only pass because a ceiling fired, never because the child happened to exit.
    const LIVE_CHILD_SECONDS: u32 = 30;

    /// The shape every Zuno session runtime has, which is what makes an inline blocking call fatal.
    fn current_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime")
    }

    /// One child that lives for `seconds`, on every supported platform.
    ///
    /// `sleep` and `ping` are invoked directly rather than through a shell, so no POSIX quoting
    /// or `sh` availability is assumed on either family.
    fn sleeping_child(seconds: u32) -> Child {
        let mut command =
            tokio::process::Command::new(if cfg!(windows) { "ping" } else { "sleep" });
        if cfg!(windows) {
            command.args(["-n", &(seconds + 1).to_string(), "127.0.0.1"]);
        } else {
            command.arg(seconds.to_string());
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // Its own group, so the production request (a group kill when no guard is active)
        // reaches exactly this child and never the test runner.
        #[cfg(unix)]
        command.process_group(0);
        command.spawn().expect("stand-in child")
    }

    async fn exited_child() -> Child {
        let mut child = sleeping_child(0);
        let _status = child.wait().await.expect("the child exits on its own");
        child
    }

    /// A reader on a pipe whose write end the test keeps open, so it can only end by abort.
    fn reader_that_never_reaches_eof() -> (JoinHandle<usize>, tokio::io::DuplexStream) {
        let (mut read_half, write_half) = tokio::io::duplex(64);
        let reader = tokio::spawn(async move {
            let mut chunk = [0_u8; 8];
            read_half.read(&mut chunk).await.unwrap_or(0)
        });
        (reader, write_half)
    }

    /// A task only a free runtime worker can poll, so its counter measures worker availability.
    fn spawn_ticker(ticks: &Arc<AtomicU64>) -> JoinHandle<()> {
        let ticks = Arc::clone(ticks);
        tokio::spawn(async move {
            loop {
                ticks.fetch_add(1, Ordering::Release);
                tokio::time::sleep(TICK).await;
            }
        })
    }

    fn alive_tasks() -> usize {
        tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks()
    }

    /// The production request against a live child in its own group reaps it within the
    /// ceilings and reports the reap, not a guess.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_child_is_reaped_through_the_production_request() {
        use std::os::unix::process::ExitStatusExt as _;

        let mut child = sleeping_child(LIVE_CHILD_SECONDS);
        let started = Instant::now();

        let outcome = shutdown_contained_child::<()>(&mut child, Vec::new(), CEILINGS).await;

        assert!(
            matches!(outcome.request, ShutdownRequest::Delivered),
            "{outcome}"
        );
        assert_eq!(
            outcome.exit_status().and_then(|status| status.signal()),
            Some(rustix::process::Signal::KILL.as_raw()),
            "the child must have died by the request's own signal: {outcome}"
        );
        assert!(outcome.is_settled(), "{outcome}");
        assert!(
            started.elapsed() < CEILINGS.reap + SLACK,
            "a child that dies at once must not cost a ceiling: {:?}",
            started.elapsed()
        );
    }

    /// A child that already exited is reported as such and the process-control call is skipped.
    ///
    /// Signalling a reaped pid would be aimed at nothing, or at whatever recycled the pid.
    #[tokio::test]
    async fn an_exited_child_is_never_signalled() {
        let mut child = exited_child().await;
        let signalled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&signalled);

        let outcome =
            shutdown_contained_child_with::<(), _>(&mut child, Vec::new(), CEILINGS, move |_pid| {
                observed.store(true, Ordering::Release);
                Ok(())
            })
            .await;

        assert!(
            !signalled.load(Ordering::Acquire),
            "a process-control call was made for a child that had already been reaped"
        );
        assert!(
            matches!(outcome.request, ShutdownRequest::AlreadyExited),
            "{outcome}"
        );
        assert!(outcome.exit_status().is_some(), "{outcome}");
        assert!(outcome.is_settled(), "{outcome}");
    }

    /// The process-control call must not stop the rest of the runtime from running.
    ///
    /// On Windows the production call is `taskkill /pid N /f /t`, which spawns a process and
    /// waits for a tree walk. Every session runtime is current-thread, so an inline call would
    /// leave the provider stream reader, the other in-flight tool calls and the client event
    /// pump unpollable for the duration of a cancellation.
    #[test]
    fn the_process_control_call_runs_off_the_runtime_worker() {
        assert_eq!(
            ticks_during_an_inline_call(),
            0,
            "control: an inline blocking call must starve the current-thread runtime, otherwise \
             this measurement cannot detect one"
        );
        assert!(
            ticks_during_the_dispatch() > 1,
            "other tasks must keep being polled while the process-control call blocks"
        );
    }

    fn ticks_during_an_inline_call() -> u64 {
        let runtime = current_thread_runtime();
        let ticks = Arc::new(AtomicU64::new(0));
        let observed = runtime.block_on(async {
            let ticker = spawn_ticker(&ticks);
            tokio::task::yield_now().await;
            let before = ticks.load(Ordering::Acquire);
            std::thread::sleep(BLOCKING_CALL);
            let after = ticks.load(Ordering::Acquire);
            ticker.abort();
            after - before
        });
        runtime.shutdown_background();
        observed
    }

    fn ticks_during_the_dispatch() -> u64 {
        let runtime = current_thread_runtime();
        let ticks = Arc::new(AtomicU64::new(0));
        let observed = runtime.block_on(async {
            let mut child = sleeping_child(LIVE_CHILD_SECONDS);
            let ticker = spawn_ticker(&ticks);
            tokio::task::yield_now().await;
            let before = ticks.load(Ordering::Acquire);
            let during = Arc::new(AtomicU64::new(0));
            let measure = Arc::clone(&during);
            let counter = Arc::clone(&ticks);
            let outcome = shutdown_contained_child_with::<(), _>(
                &mut child,
                Vec::new(),
                CEILINGS,
                move |_pid| {
                    std::thread::sleep(BLOCKING_CALL);
                    measure.store(counter.load(Ordering::Acquire), Ordering::Release);
                    Ok(())
                },
            )
            .await;
            ticker.abort();
            assert!(
                matches!(outcome.request, ShutdownRequest::Delivered),
                "{outcome}"
            );
            during.load(Ordering::Acquire) - before
        });
        runtime.shutdown_background();
        observed
    }

    /// Moving the call off the worker does not make it finish, so its join must be bounded, and
    /// the reap that follows must be bounded too when the child never dies.
    #[test]
    fn a_wedged_process_control_call_ends_at_the_ceilings() {
        let runtime = current_thread_runtime();
        let (outcome, elapsed) = runtime.block_on(async {
            let mut child = sleeping_child(LIVE_CHILD_SECONDS);
            let started = Instant::now();
            let outcome = tokio::time::timeout(
                CEILINGS.process_control + CEILINGS.reap + CEILINGS.drain + SLACK,
                shutdown_contained_child_with::<(), _>(&mut child, Vec::new(), CEILINGS, |_pid| {
                    std::thread::sleep(WEDGED_CALL);
                    Ok(())
                }),
            )
            .await
            .expect("the dispatch must settle even when the process-control call never returns");
            (outcome, started.elapsed())
        });
        runtime.shutdown_background();

        assert!(
            matches!(outcome.request, ShutdownRequest::TimedOut),
            "{outcome}"
        );
        assert!(matches!(outcome.reap, ChildReap::TimedOut), "{outcome}");
        assert!(!outcome.is_settled(), "{outcome}");
        assert!(
            elapsed >= CEILINGS.process_control + CEILINGS.reap,
            "both ceilings must have been paid, not skipped: {elapsed:?}"
        );
    }

    /// A reader still running at the drain ceiling is aborted, so the runtime stops counting
    /// it; dropping its handle would have left it alive for the life of the pipe's writer.
    #[tokio::test]
    async fn a_reader_that_never_reaches_eof_is_aborted_not_dropped() {
        let mut child = exited_child().await;
        let baseline = alive_tasks();
        let (reader, write_half) = reader_that_never_reaches_eof();
        assert_eq!(alive_tasks(), baseline + 1);

        let outcome = shutdown_contained_child(&mut child, vec![reader], CEILINGS).await;

        assert_eq!(
            outcome.readers,
            ReaderDrain {
                finished: 0,
                aborted: 1
            },
            "{outcome}"
        );
        assert!(outcome.outputs.iter().all(Option::is_none), "{outcome}");
        assert!(!outcome.is_settled(), "{outcome}");
        // The abort takes effect when the scheduler next runs; give it that turn.
        tokio::time::timeout(SLACK, async {
            while alive_tasks() != baseline {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("an aborted reader must leave the runtime, not linger detached");
        drop(write_half);
    }

    /// A reader that finishes within the ceiling hands its output back in order.
    #[tokio::test]
    async fn a_reader_that_finishes_returns_its_output_in_order() {
        let mut child = exited_child().await;
        let first = tokio::spawn(async { "first stderr".to_owned() });
        let second = tokio::spawn(async { "second stderr".to_owned() });

        let outcome = shutdown_contained_child(&mut child, vec![first, second], CEILINGS).await;

        assert_eq!(
            outcome.readers,
            ReaderDrain {
                finished: 2,
                aborted: 0
            },
            "{outcome}"
        );
        assert_eq!(
            outcome.outputs,
            vec![
                Some("first stderr".to_owned()),
                Some("second stderr".to_owned())
            ]
        );
        assert!(outcome.is_settled(), "{outcome}");
    }

    /// The drain ceiling is one deadline for every reader, not one per reader.
    ///
    /// Three readers that never finish must cost one ceiling, so the bound a call site states
    /// is the bound it gets however many pipes the child had.
    #[tokio::test]
    async fn the_drain_ceiling_is_paid_once_for_all_readers() {
        const DRAIN: Duration = Duration::from_secs(1);

        let readers = (0..3).map(|_| reader_that_never_reaches_eof());
        let (readers, write_halves): (Vec<_>, Vec<_>) = readers.unzip();
        let started = Instant::now();

        let (drain, outputs) = drain_readers(DRAIN, readers).await;
        let elapsed = started.elapsed();

        assert_eq!(
            drain,
            ReaderDrain {
                finished: 0,
                aborted: 3
            }
        );
        assert_eq!(outputs.len(), 3);
        assert!(
            elapsed >= DRAIN,
            "the readers cannot have been aborted before the ceiling: {elapsed:?}"
        );
        assert!(
            elapsed < DRAIN * 2 + DRAIN / 2,
            "three readers cost {elapsed:?} against a {DRAIN:?} ceiling, so the ceiling is \
             being paid per reader rather than once"
        );
        drop(write_halves);
    }

    /// The off-worker wrapper reports a value that arrives in time and nothing otherwise.
    ///
    /// A manual runtime, shut down in the background: dropping a runtime waits for every
    /// blocking task that has started, and the wedged call here is meant to outlive the test.
    #[test]
    fn off_runtime_worker_reports_only_what_arrived_within_the_ceiling() {
        let runtime = current_thread_runtime();
        let (prompt, late, elapsed) = runtime.block_on(async {
            let prompt = off_runtime_worker(CEILINGS.process_control, || 7_u8).await;
            let started = Instant::now();
            let late = off_runtime_worker(CEILINGS.process_control, || {
                std::thread::sleep(WEDGED_CALL);
                7_u8
            })
            .await;
            (prompt, late, started.elapsed())
        });
        runtime.shutdown_background();

        assert_eq!(prompt, Some(7));
        assert_eq!(late, None);
        assert!(
            elapsed < WEDGED_CALL,
            "the ceiling must release the await before the call returns: {elapsed:?}"
        );
    }
}
