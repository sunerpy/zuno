//! One PTY session: its identity, its child process, and its output fan-out.
//!
//! # Threads, not tasks
//!
//! `portable_pty`'s reader is a blocking [`std::io::Read`] and its child wait is a
//! blocking `wait()`. Neither has an async form, so each session owns two plain OS
//! threads — one draining output into the scrollback, one waiting for the child —
//! and every publish is a non-blocking `try_send`. That keeps the crate
//! constructible and usable with no Tokio runtime in scope, and it is why
//! [`crate::PtyService`] has no `async fn`.
//!
//! # The ordering contract: every chunk precedes `Ended`
//!
//! [`PtyOutput::Ended`] is the stop signal — a consumer that sees it stops reading,
//! and wave 9's WebSocket route will close on it. So `Ended` must be the *last*
//! thing a subscriber receives, not merely *a* thing it receives.
//!
//! The two threads make that a race unless it is enforced. A child's exit and the
//! pty's remaining buffered output are independent events: `child.wait()` returns as
//! soon as the process dies, while the bytes it wrote moments earlier are still in
//! the kernel's pty buffer waiting for the reader. Publishing `Ended` on the
//! waiter's own schedule therefore truncates the tail of every short-lived command
//! — `ls` in a terminal showing nothing at all, intermittently.
//!
//! The waiter closes the gap by first closing Zuno's writable and master handles,
//! then waiting for the reader to reach end-of-output before it marks the session
//! exited. Closing those handles is load-bearing on Windows: ConPTY does not produce
//! EOF while a master or writer is still alive, so waiting for the reader before
//! closing them is a circular wait. The reader raises [`DrainGate`] only after its
//! final `ingest` has returned, so observing that flag means every chunk is already
//! queued ahead of anything sent later. The wait is bounded by [`DRAIN_GRACE`]
//! because the pty can outlive the child — see that constant.
//!
//! ConPTY also starts a pseudoconsole created with inherited-cursor support by
//! asking the host terminal for its cursor position (`ESC[6n`). Zuno is the host,
//! not a terminal emulator, so the reader consumes that one startup query and
//! answers `ESC[1;1R` on the input side before exposing output. Without the reply,
//! `cmd.exe` and PowerShell can remain blocked before processing the first command.
//!
//! # Why output is dropped rather than queued
//!
//! A subscriber's channel is bounded. When it fills, chunks are discarded and
//! counted, and the subscriber is told with [`PtyOutput::Lagged`] carrying the
//! cursor to resume from. Growing the channel instead would move this crate's
//! whole reason for existing — an unbounded buffer — one layer down, which is the
//! mistake todo 50 documented for `zuno-watch`. Dropping is safe *here* and not in a
//! filesystem watcher because the scrollback is the durable copy: a lagging client
//! re-reads the window it missed with [`crate::PtyService::retained_output`].
//!
//! The oracle instead stages output in a per-subscriber `pending: string[]`
//! (`packages/core/src/pty.ts:21-27`) that nothing bounds, so a client that
//! connects and never reads accumulates the session's entire output a second time.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::buffer::{Replay, ReplayCursor, ScrollbackBuffer};
use crate::{BoxSource, PtyError, shells};

/// Bytes read from the pty in one syscall.
///
/// Also the unit of a subscriber's queue slot, so it sets the per-attachment
/// memory bound together with [`DEFAULT_SUBSCRIBER_CAPACITY`].
const READ_CHUNK: usize = 8 * 1024;

/// Queued output chunks per attachment before older ones are dropped.
///
/// 256 slots at [`READ_CHUNK`] bytes is a 2 MiB ceiling per attachment, matching
/// the scrollback's own bound: a subscriber cannot cost more than the history it
/// could re-read anyway. It also absorbs a full `clear && cat large-file` burst
/// without dropping anything on a consumer that is merely slow rather than stuck.
pub const DEFAULT_SUBSCRIBER_CAPACITY: usize = 256;

/// How long the waiter gives the reader to drain the pty after the child exits.
///
/// A bound is mandatory, not defensive: the pty outlives the child whenever a
/// grandchild inherited it (`sh -c 'server & exit 0'` is the ordinary case), and the
/// reader then blocks in `read` for as long as that grandchild lives. An unbounded
/// wait would defer the exit — and with it the retention eviction and every
/// subscriber's `Ended` — for the grandchild's whole lifetime.
///
/// 500 ms is ~3 orders of magnitude more than the drain needs. What is left to read
/// once the child is dead is at most the kernel's pty buffer (64 KiB on Linux),
/// which is 8 reads of [`READ_CHUNK`] and a memcpy each. The headroom is entirely
/// for scheduler delay under load. Exceeding it is logged, not silent, because the
/// only thing lost is the ordering guarantee for one session's tail.
pub const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Terminal type advertised to the child (`packages/core/src/pty.ts:177`).
const TERM_VALUE: &str = "xterm-256color";

/// Marks a shell as running inside Zuno.
const TERMINAL_MARKER_ENV: &str = "ZUNO_TERMINAL";

#[cfg(any(windows, test))]
const STARTUP_CURSOR_QUERY: &[u8] = b"\x1b[6n";

#[cfg(windows)]
const STARTUP_CURSOR_RESPONSE: &[u8] = b"\x1b[1;1R";

#[cfg(any(windows, test))]
#[derive(Debug, Default)]
struct StartupCursorNegotiation {
    pending: Vec<u8>,
    complete: bool,
}

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
struct StartupCursorOutput {
    visible: Vec<u8>,
    query_offset: Option<usize>,
}

#[cfg(any(windows, test))]
impl StartupCursorNegotiation {
    fn push(&mut self, chunk: &[u8]) -> StartupCursorOutput {
        if self.complete {
            return StartupCursorOutput {
                visible: chunk.to_vec(),
                query_offset: None,
            };
        }

        self.pending.extend_from_slice(chunk);
        if let Some(query_offset) = self
            .pending
            .windows(STARTUP_CURSOR_QUERY.len())
            .position(|window| window == STARTUP_CURSOR_QUERY)
        {
            let mut visible = Vec::with_capacity(self.pending.len() - STARTUP_CURSOR_QUERY.len());
            visible.extend_from_slice(&self.pending[..query_offset]);
            visible.extend_from_slice(&self.pending[query_offset + STARTUP_CURSOR_QUERY.len()..]);
            self.pending.clear();
            self.complete = true;
            return StartupCursorOutput {
                visible,
                query_offset: Some(query_offset),
            };
        }

        let retained = (1..STARTUP_CURSOR_QUERY.len())
            .rev()
            .find(|length| self.pending.ends_with(&STARTUP_CURSOR_QUERY[..*length]))
            .unwrap_or(0);
        let flush = self.pending.len().saturating_sub(retained);
        StartupCursorOutput {
            visible: self.pending.drain(..flush).collect(),
            query_offset: None,
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

/// A PTY session identifier, `pty_`-prefixed as `packages/schema/src/pty.ts:9`.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct PtyId(String);

/// Millisecond clock reading of the last minted id, so ids strictly ascend.
static LAST_MINTED_MILLIS: AtomicU64 = AtomicU64::new(0);

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

impl PtyId {
    /// Mints an ascending identifier, as `PtyID.ascending()` does at
    /// `packages/schema/src/pty.ts:13-14`.
    ///
    /// Shape is the oracle's: `pty_` then 12 lowercase hex digits of the mint-time
    /// millisecond, then 14 base62 characters of randomness
    /// (`packages/schema/src/identifier.ts`). The timestamp is bumped past the
    /// previous reading when the clock has not advanced, so two ids minted in the
    /// same millisecond still sort by creation — which is the whole point of the
    /// `ascending` name and is what makes a lexical sort of session ids stable.
    #[must_use]
    pub fn mint() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                elapsed.as_millis().min(u128::from(u64::MAX)) as u64
            });
        let millis = LAST_MINTED_MILLIS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| {
                Some(if now > last {
                    now
                } else {
                    last.saturating_add(1)
                })
            })
            .map_or(now, |previous| {
                if now > previous {
                    now
                } else {
                    previous.saturating_add(1)
                }
            });

        let mut id = format!("pty_{millis:012x}");
        for byte in uuid::Uuid::new_v4().as_bytes().iter().take(14) {
            id.push(char::from(BASE62[usize::from(*byte) % BASE62.len()]));
        }
        Self(id)
    }

    /// Wraps an identifier supplied by a caller, such as an HTTP path parameter.
    #[must_use]
    pub fn from_raw(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as it appears on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PtyId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Whether a session's child is still running (`packages/schema/src/pty.ts:24`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PtyStatus {
    /// The child is alive.
    Running,
    /// The child exited; status, exit code and retained output remain readable
    /// until the session is removed or evicted by [`crate::retention`].
    ///
    /// **Once this status is observable, `PtyEvent::Exited` has already been
    /// broadcast.** So a consumer may read the status first and subscribe or drain
    /// afterwards without missing the exit code. See
    /// [`SessionShared::mark_exited`](crate::session) for how that is enforced.
    Exited,
}

/// A session as clients see it, from `packages/schema/src/pty.ts:20-29`.
///
/// Deliberately carries no size: the oracle's `Info` has no `rows`/`cols` either,
/// because the kernel owns the winsize and a stale copy here would be a second
/// source of truth. A caller that needs it asks the pty.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyInfo {
    /// The session identifier.
    pub id: PtyId,
    /// Display title, defaulted from the id's last four characters.
    pub title: String,
    /// Resolved absolute path of the spawned program.
    pub command: String,
    /// Arguments as spawned, including an appended `-l` for a login shell.
    pub args: Vec<String>,
    /// Working directory the child started in.
    pub cwd: String,
    /// Whether the child is alive.
    pub status: PtyStatus,
    /// The child's process id.
    pub pid: u32,
    /// Present once the child exited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
}

/// A terminal's visible size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalSize {
    /// Lines of text.
    pub rows: u16,
    /// Columns of text.
    pub cols: u16,
}

impl Default for TerminalSize {
    /// `portable_pty`'s own default, and the conventional terminal size.
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

impl From<TerminalSize> for PtySize {
    fn from(size: TerminalSize) -> Self {
        Self {
            rows: size.rows.max(1),
            cols: size.cols.max(1),
            // The oracle never sets these either; a cell's pixel size is only
            // consulted by programs drawing sixel graphics.
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// Request body of `POST /pty`, from `packages/schema/src/pty.ts:40-46`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CreateInput {
    /// Program to run. Defaults to the preferred shell.
    pub command: Option<String>,
    /// Arguments before any login flag is appended.
    pub args: Option<Vec<String>>,
    /// Working directory. Defaults to the service's directory.
    pub cwd: Option<String>,
    /// Display title. Defaults to `Terminal <last four id characters>`.
    pub title: Option<String>,
    /// Extra environment entries, applied over the parent's.
    pub env: Option<HashMap<String, String>>,
    /// Initial size. Absent means [`TerminalSize::default`].
    ///
    /// Not in the oracle's `CreateInput`, which always opens at the pty library's
    /// default and waits for the client's first resize. Accepting it here avoids a
    /// visible reflow of the shell prompt on connect.
    pub size: Option<TerminalSize>,
}

/// Request body of `PUT /pty/:ptyID`, from `packages/schema/src/pty.ts:49-57`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateInput {
    /// New display title.
    pub title: Option<String>,
    /// New size, applied to the kernel's winsize.
    pub size: Option<TerminalSize>,
}

/// What a subscriber receives from an attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyOutput {
    /// Terminal bytes, in order, starting at the attachment's cursor.
    Chunk(Vec<u8>),
    /// The subscriber fell behind and `dropped` bytes were discarded.
    ///
    /// Delivered *before* the chunk that follows the gap, so a client learns its
    /// view has a hole before it renders a discontinuity. Re-read the window with
    /// [`crate::PtyService::retained_output`] from `cursor`, which is the absolute
    /// cursor of the next chunk.
    Lagged {
        /// Bytes discarded since the previous `Lagged`.
        dropped: u64,
        /// Absolute cursor the next [`PtyOutput::Chunk`] starts at.
        cursor: u64,
    },
    /// No further output will arrive: the child exited, or the session was
    /// removed or torn down. `exit_code` is absent for the latter two.
    ///
    /// **Always the last event a subscriber receives, and for a child that exited on
    /// its own, it arrives after every chunk that child produced.** A consumer may
    /// therefore stop reading here without truncating the output — see the module
    /// docs for how the two threads are ordered to make that true, and
    /// [`DRAIN_GRACE`] for the one case where the guarantee degrades (a grandchild
    /// holding the pty open, which is logged).
    ///
    /// The completeness half does not apply when the session was removed or the
    /// service dropped: that is a caller asking for the terminal to go away, so its
    /// unread tail goes with it. `exit_code: None` distinguishes the two.
    Ended {
        /// The child's exit code, when it exited on its own.
        exit_code: Option<u32>,
    },
}

/// A live subscription to one session's output.
///
/// Detaches on drop, mirroring the oracle's `Attachment.detach` (`pty.ts:262-267`)
/// without requiring a caller to remember to call it — a WebSocket handler that
/// panics mid-frame would otherwise leak a subscriber for the session's lifetime.
#[derive(Debug)]
pub struct Attachment {
    /// Retained output from the requested cursor to the attachment point.
    pub replay: Vec<u8>,
    /// Absolute cursor after [`Self::replay`]. Live output continues from here.
    pub cursor: u64,
    /// Live output. Bounded; see [`PtyOutput::Lagged`].
    pub output: Receiver<PtyOutput>,
    token: u64,
    shared: Arc<SessionShared>,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.shared.detach(self.token);
    }
}

/// How a subscription starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachOptions {
    /// Where the replay begins.
    pub cursor: ReplayCursor,
    /// Queue slots before output is dropped for this subscriber.
    pub capacity: usize,
}

impl Default for AttachOptions {
    fn default() -> Self {
        Self {
            cursor: ReplayCursor::Full,
            capacity: DEFAULT_SUBSCRIBER_CAPACITY,
        }
    }
}

/// A snapshot of one session's retained output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedOutput {
    /// Every byte still retained, oldest first.
    pub bytes: Vec<u8>,
    /// Absolute cursor of the first retained byte.
    pub start_cursor: u64,
    /// Absolute cursor just past the last retained byte.
    pub end_cursor: u64,
    /// Total bytes the child ever produced, retained or not.
    pub total_written: u64,
    /// Bytes discarded to stay within the scrollback limit.
    pub discarded: u64,
    /// The scrollback's ceiling, so a caller can assert against it.
    pub limit: usize,
}

/// Per-session tunables, so tests can reach the bounded paths cheaply.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Scrollback ceiling in bytes.
    pub buffer_limit: usize,
    /// Directory for a [`CreateInput`] that does not name one.
    pub default_cwd: PathBuf,
    /// The `shell` config value, consulted when no command is given.
    pub configured_shell: Option<String>,
}

/// Reports a session's exit, split into two phases by the lock each may take.
///
/// The split is what makes the visibility guarantee on [`PtyStatus::Exited`]
/// enforceable: [`Self::announce`] must run under the session's state lock (see
/// [`SessionShared::mark_exited`]) while [`Self::record`] must not, because it takes
/// the registry lock and `PtyService::list` already holds registry-then-session.
/// Collapsing them back into one callback reintroduces either the lost event or a
/// lock-order inversion.
pub(crate) trait ExitObserver: Send + Sync {
    /// Broadcasts the exit. Invoked while the session's state lock is held, so it
    /// must not block and must not acquire the registry lock.
    fn announce(&self, id: &PtyId, exit_code: Option<u32>);

    /// Retention bookkeeping and cap eviction. Invoked with no session lock held.
    fn record(&self, id: &PtyId);
}

#[derive(Debug)]
struct Subscriber {
    sender: Sender<PtyOutput>,
    dropped: u64,
    closed: bool,
}

#[derive(Debug)]
struct SessionState {
    info: PtyInfo,
    buffer: ScrollbackBuffer,
    subscribers: HashMap<u64, Subscriber>,
    next_token: u64,
    /// Set once the session has left the registry, so a later exit stays quiet.
    ///
    /// Without it, a session removed while still running would announce `Exited`
    /// after its `Deleted` — an event for an id no lookup can reach, arriving after
    /// the event that said it was gone. Guarded here rather than by re-checking the
    /// registry because this flag and the status transition share one lock, and a
    /// registry check would be a second racy read.
    detached: bool,
}

/// The reader's "I have reached end-of-output" latch, awaited by the waiter.
///
/// Deliberately a mutex of its own rather than a field of [`SessionState`]: the
/// waiter blocks on this one, and blocking on the state mutex would stall `ingest`
/// — the very thing it is waiting for. No code path takes both locks at once, so
/// there is no order to invert.
#[derive(Debug, Default)]
struct DrainGate {
    drained: Mutex<bool>,
    signal: Condvar,
}

impl DrainGate {
    /// Latches end-of-output and wakes the waiter.
    ///
    /// Must be called only after the last `ingest` has *returned*. That is what
    /// makes the latch mean "every chunk is queued" rather than "no more will be
    /// read".
    fn mark_drained(&self) {
        let mut drained = self
            .drained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *drained = true;
        self.signal.notify_all();
    }

    /// Waits up to `grace` for end-of-output, returning whether it was reached.
    fn wait_for_drain(&self, grace: Duration) -> bool {
        let drained = self
            .drained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (drained, _timed_out) = self
            .signal
            .wait_timeout_while(drained, grace, |drained| !*drained)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *drained
    }
}

/// The part of a session its reader and waiter threads share.
#[derive(Debug)]
pub(crate) struct SessionShared {
    state: Mutex<SessionState>,
    drain: DrainGate,
}

impl SessionShared {
    fn lock(&self) -> MutexGuard<'_, SessionState> {
        // A panic in a subscriber's `try_send` would otherwise stop every client
        // of this session hearing anything ever again. The guarded region is a
        // byte ring and a map of channel senders, both of which stay structurally
        // valid, so recovering is strictly better than propagating.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn ingest(&self, chunk: &[u8]) {
        let mut state = self.lock();
        let chunk_start = state.buffer.end_cursor();
        state.buffer.push(chunk);

        for subscriber in state.subscribers.values_mut() {
            deliver(subscriber, chunk, chunk_start);
        }
        state.subscribers.retain(|_, subscriber| !subscriber.closed);
    }

    /// Records the child's exit and announces it, atomically. Returns whether this
    /// call was the transition.
    ///
    /// Guards against a second notification the way `pty.ts:224` does, so a removal
    /// racing a natural exit cannot report two exits for one session.
    ///
    /// # Why `announce` is called with the lock held
    ///
    /// This lock is the one every status reader must take, so calling `announce`
    /// inside it establishes a happens-before: any thread that can observe
    /// `status == Exited` — through `PtyService::get`, `list`, or `attach` — is
    /// necessarily running after `PtyEvent::Exited` reached the broadcast channel.
    /// That is the guarantee documented on [`PtyStatus::Exited`], and a consumer
    /// pairing `GET /pty/:ptyID` with the event stream depends on it: without it,
    /// seeing `exited` from the API and *then* reading the stream can miss the exit
    /// code entirely.
    ///
    /// Announcing after the lock is released is not merely late. Measured on this
    /// host, 1,280 iterations of "wait for `Exited` status, remove, read the
    /// events": 7 lost the event permanently and 3 saw it late, because a `remove`
    /// landing in the gap made the registry-side publish skip itself. So the lock is
    /// load-bearing, not tidiness.
    fn mark_exited(&self, exit_code: Option<u32>, announce: &dyn Fn(Option<u32>)) -> bool {
        let mut state = self.lock();
        if state.info.status == PtyStatus::Exited {
            return false;
        }
        state.info.status = PtyStatus::Exited;
        state.info.exit_code = exit_code;
        end_subscribers(&mut state, exit_code);
        if state.detached {
            return false;
        }
        announce(exit_code);
        true
    }

    /// Ends every subscription and marks the session as gone from the registry.
    ///
    /// Matches `teardown` (`pty.ts:121-129`), which notifies with no exit code and
    /// leaves `status` alone: the session is going away, so its status is about to
    /// stop being observable rather than becoming `exited`. The `detached` flag is
    /// set in the same lock acquisition, which is what stops a still-running child's
    /// eventual exit from being announced after the session's `Deleted`.
    fn detach_all(&self) {
        let mut state = self.lock();
        state.detached = true;
        end_subscribers(&mut state, None);
    }

    fn info(&self) -> PtyInfo {
        self.lock().info.clone()
    }

    fn set_title(&self, title: String) {
        self.lock().info.title = title;
    }

    fn retained_output(&self) -> RetainedOutput {
        let state = self.lock();
        RetainedOutput {
            bytes: state.buffer.to_bytes(),
            start_cursor: state.buffer.start_cursor(),
            end_cursor: state.buffer.end_cursor(),
            total_written: state.buffer.total_written(),
            discarded: state.buffer.discarded(),
            limit: state.buffer.limit(),
        }
    }

    fn reserved_bytes(&self) -> usize {
        self.lock().buffer.reserved_bytes()
    }

    fn subscriber_count(&self) -> usize {
        self.lock().subscribers.len()
    }

    /// Registers a subscriber and computes its replay under one lock.
    ///
    /// Holding the lock across both is what makes the handover exact: the reader
    /// thread needs the same lock to append, so no chunk can land between the
    /// replay snapshot and the subscription. That is what lets this crate drop the
    /// oracle's `activate()` step and its unbounded `pending` staging array
    /// (`pty.ts:252-261`) rather than reimplement them.
    fn attach(self: &Arc<Self>, options: AttachOptions) -> Result<Attachment, PtyError> {
        let mut state = self.lock();
        if state.info.status == PtyStatus::Exited {
            return Err(PtyError::Exited {
                id: state.info.id.clone(),
            });
        }

        let Replay { bytes, cursor } = state.buffer.replay(options.cursor);
        let (sender, output) = mpsc::channel(options.capacity.max(1));
        let token = state.next_token;
        state.next_token = state.next_token.saturating_add(1);
        state.subscribers.insert(
            token,
            Subscriber {
                sender,
                dropped: 0,
                closed: false,
            },
        );

        Ok(Attachment {
            replay: bytes,
            cursor,
            output,
            token,
            shared: Arc::clone(self),
        })
    }

    fn detach(&self, token: u64) {
        self.lock().subscribers.remove(&token);
    }
}

fn end_subscribers(state: &mut SessionState, exit_code: Option<u32>) {
    for subscriber in state.subscribers.values() {
        if subscriber.closed {
            continue;
        }
        // Best effort: a subscriber whose queue is full still learns the session
        // ended when it drains, because the receiver observes the closed channel
        // once the sender is dropped below.
        let _outcome = subscriber.sender.try_send(PtyOutput::Ended { exit_code });
    }
    state.subscribers.clear();
}

fn deliver(subscriber: &mut Subscriber, chunk: &[u8], chunk_start: u64) {
    if subscriber.closed {
        return;
    }
    if subscriber.dropped > 0 {
        match subscriber.sender.try_send(PtyOutput::Lagged {
            dropped: subscriber.dropped,
            cursor: chunk_start,
        }) {
            Ok(()) => subscriber.dropped = 0,
            Err(TrySendError::Full(_)) => {
                subscriber.dropped = subscriber.dropped.saturating_add(chunk.len() as u64);
                return;
            }
            Err(TrySendError::Closed(_)) => {
                subscriber.closed = true;
                return;
            }
        }
    }
    match subscriber.sender.try_send(PtyOutput::Chunk(chunk.to_vec())) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            subscriber.dropped = subscriber.dropped.saturating_add(chunk.len() as u64);
        }
        Err(TrySendError::Closed(_)) => subscriber.closed = true,
    }
}

/// A running session's operable handle.
///
/// Every I/O field a caller can act on sits behind its own mutex, so a resize does
/// not wait on a write and neither waits on the reader thread appending output.
pub(crate) struct SessionHandle {
    id: PtyId,
    shared: Arc<SessionShared>,
    io: Arc<SessionIo>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

/// The two host-side handles whose lifetime controls end-of-output.
///
/// `portable-pty` documents this lifetime requirement through its Windows support:
/// a ConPTY reader does not observe EOF while writable/master handles remain alive.
/// The child waiter therefore shares this object with [`SessionHandle`] and closes
/// it after `child.wait()` returns. Keeping the two handles in `Option`s makes that
/// close atomic with respect to later writes/resizes and, importantly, gives the
/// waiter ownership of the drop instead of leaving it to registry eviction.
struct SessionIo {
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    last_size: Mutex<TerminalSize>,
}

impl std::fmt::Debug for SessionIo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SessionIo").finish_non_exhaustive()
    }
}

impl SessionIo {
    fn close_writer(&self) {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Closes input before the master, matching the PTY backend's EOF contract.
    fn close(&self) {
        self.close_writer();
        self.master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    fn write(&self, data: &[u8]) -> std::io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(writer) = writer.as_mut() else {
            return Ok(());
        };
        writer.write_all(data).and_then(|()| writer.flush())
    }

    fn resize(&self, size: TerminalSize) -> Result<(), BoxSource> {
        let master = self
            .master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(master) = master.as_ref() else {
            return Ok(());
        };
        master
            .resize(PtySize::from(size))
            .map_err(BoxSource::from)?;
        *self
            .last_size
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = size;
        Ok(())
    }

    fn size(&self) -> Result<TerminalSize, BoxSource> {
        let master = self
            .master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(master) = master.as_ref() else {
            return Ok(*self
                .last_size
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner));
        };
        let size = master.get_size().map_err(BoxSource::from)?;
        let size = TerminalSize {
            rows: size.rows,
            cols: size.cols,
        };
        *self
            .last_size
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = size;
        Ok(size)
    }
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionHandle")
            .field("id", &self.id)
            .field("info", &self.shared.info())
            .finish_non_exhaustive()
    }
}

impl SessionHandle {
    /// Opens a pty, spawns the child into it, and starts its two threads.
    ///
    /// # Errors
    ///
    /// [`PtyError::Shell`] when the configured default shell is invalid,
    /// [`PtyError::Open`] when the platform refuses a new pty (typically the
    /// per-user pty limit), and [`PtyError::Spawn`] when the command cannot be
    /// executed in it.
    pub(crate) fn spawn(
        input: CreateInput,
        options: &SessionOptions,
        on_exit: Arc<dyn ExitObserver>,
    ) -> Result<Arc<Self>, PtyError> {
        let id = PtyId::mint();
        let command = match input.command.filter(|command| !command.is_empty()) {
            Some(command) => PathBuf::from(command),
            None => shells::preferred(options.configured_shell.as_deref())
                .map_err(|source| PtyError::Shell { source })?,
        };
        let command_display = command.to_string_lossy().into_owned();

        let mut args = input.args.unwrap_or_default();
        if shells::login(&command) {
            // `pty.ts:175`: a terminal wants the user's profile sourced.
            args.push("-l".to_owned());
        }
        let cwd = input
            .cwd
            .filter(|cwd| !cwd.is_empty())
            .map_or_else(|| options.default_cwd.clone(), PathBuf::from);

        let terminal_size = input.size.unwrap_or_default();
        let size = PtySize::from(terminal_size);
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|source| PtyError::Open {
                command: command_display.clone(),
                source: BoxSource::from(source),
            })?;

        let (guarded_program, guarded_arguments) =
            zuno_process::guarded_terminal_argv(&command, &args);
        let mut builder = CommandBuilder::new(guarded_program);
        builder.args(guarded_arguments);
        builder.cwd(&cwd);
        for (key, value) in input.env.unwrap_or_default() {
            builder.env(key, value);
        }
        builder.env("TERM", TERM_VALUE);
        builder.env(TERMINAL_MARKER_ENV, "1");
        if cfg!(windows) {
            // `pty.ts:180-184`: ConPTY hands the child the console code page, which
            // is not UTF-8 by default, so a shell would mangle non-ASCII output.
            for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
                builder.env(key, "C.UTF-8");
            }
        }

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|source| PtyError::Spawn {
                command: command_display.clone(),
                source: BoxSource::from(source),
            })?;
        // The slave fd must go before anything reads the master: while it is open
        // the kernel keeps the pty writable, so the reader never observes EOF and
        // the reader thread would outlive the child forever.
        drop(pair.slave);

        let pid = child.process_id().unwrap_or_default();
        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|source| PtyError::Open {
                command: command_display.clone(),
                source: BoxSource::from(source),
            })?;
        let writer = pair.master.take_writer().map_err(|source| PtyError::Open {
            command: command_display.clone(),
            source: BoxSource::from(source),
        })?;

        let title = input
            .title
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| {
                let suffix: String = id
                    .as_str()
                    .chars()
                    .rev()
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("Terminal {suffix}")
            });
        let shared = Arc::new(SessionShared {
            drain: DrainGate::default(),
            state: Mutex::new(SessionState {
                info: PtyInfo {
                    id: id.clone(),
                    title,
                    command: command_display,
                    args,
                    cwd: cwd.to_string_lossy().into_owned(),
                    status: PtyStatus::Running,
                    pid,
                    exit_code: None,
                },
                buffer: ScrollbackBuffer::with_limit(options.buffer_limit),
                subscribers: HashMap::new(),
                next_token: 0,
                detached: false,
            }),
        });
        let io = Arc::new(SessionIo {
            writer: Mutex::new(Some(writer)),
            master: Mutex::new(Some(pair.master)),
            last_size: Mutex::new(terminal_size),
        });

        // Reader first: the waiter awaits its drain latch, so it must exist (or have
        // failed to start, which releases the latch) before the waiter can observe it.
        spawn_reader(&id, &shared, Arc::clone(&io), reader);
        spawn_waiter(&id, Arc::clone(&shared), Arc::clone(&io), child, on_exit);

        Ok(Arc::new(Self {
            id,
            shared,
            io,
            killer: Mutex::new(killer),
        }))
    }

    pub(crate) fn id(&self) -> &PtyId {
        &self.id
    }

    pub(crate) fn info(&self) -> PtyInfo {
        self.shared.info()
    }

    pub(crate) fn retained_output(&self) -> RetainedOutput {
        self.shared.retained_output()
    }

    pub(crate) fn reserved_bytes(&self) -> usize {
        self.shared.reserved_bytes()
    }

    pub(crate) fn subscriber_count(&self) -> usize {
        self.shared.subscriber_count()
    }

    pub(crate) fn attach(&self, options: AttachOptions) -> Result<Attachment, PtyError> {
        self.shared.attach(options)
    }

    pub(crate) fn set_title(&self, title: String) {
        self.shared.set_title(title);
    }

    /// Writes terminal input. A no-op once the child exited, as at `pty.ts:200`.
    pub(crate) fn write(&self, data: &[u8]) -> Result<(), PtyError> {
        if self.shared.info().status == PtyStatus::Exited {
            return Ok(());
        }
        self.io.write(data).map_err(|source| PtyError::Write {
            id: self.id.clone(),
            source,
        })
    }

    /// Updates the kernel's winsize, which signals `SIGWINCH` to the child.
    ///
    /// A no-op once the child exited, matching the `status === "running"` guard at
    /// `pty.ts:194`.
    pub(crate) fn resize(&self, size: TerminalSize) -> Result<(), PtyError> {
        if self.shared.info().status == PtyStatus::Exited {
            return Ok(());
        }
        self.io.resize(size).map_err(|source| PtyError::Resize {
            id: self.id.clone(),
            source,
        })
    }

    /// The size the kernel currently reports, for verifying a resize landed.
    ///
    /// # Errors
    ///
    /// [`PtyError::Resize`] when the platform cannot report the size.
    pub(crate) fn size(&self) -> Result<TerminalSize, PtyError> {
        self.io.size().map_err(|source| PtyError::Resize {
            id: self.id.clone(),
            source,
        })
    }

    /// Requests shutdown from a still-running child and ends every subscription.
    ///
    /// Mirrors `teardown` (`pty.ts:121-129`). Non-blocking: it signals and returns
    /// rather than reaping, and the session's own waiter thread observes the death
    /// and finishes. Signalling failure falls back to the PTY backend's kill;
    /// failure there is ignored because the child either already died or is
    /// unkillable, and neither leaves
    /// anything for a caller to do.
    pub(crate) fn shutdown(&self) {
        if self.shared.info().status == PtyStatus::Running {
            let pid = self.shared.info().pid;
            if zuno_process::request_contained_process_shutdown(pid).is_err() {
                let mut killer = self
                    .killer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _outcome = killer.kill();
            }
        }
        // A writer kept alive after shutdown prevents ConPTY's output reader from
        // observing EOF. The waiter owns the master close after `child.wait()`, so
        // teardown stays non-blocking while still releasing the writable handle now.
        self.io.close_writer();
        self.shared.detach_all();
    }
}

fn spawn_reader(
    id: &PtyId,
    shared: &Arc<SessionShared>,
    io: Arc<SessionIo>,
    mut reader: Box<dyn Read + Send>,
) {
    let name = format!("zuno-pty-read-{id}");
    let owned = Arc::clone(shared);
    #[cfg(windows)]
    let reader_id = id.clone();
    let thread = std::thread::Builder::new().name(name).spawn(move || {
        let mut chunk = vec![0u8; READ_CHUNK];
        #[cfg(windows)]
        let mut startup_cursor = StartupCursorNegotiation::default();
        #[cfg(not(windows))]
        let _ = &io;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    #[cfg(windows)]
                    {
                        let mut output = startup_cursor.push(&chunk[..read]);
                        if let Some(query_offset) = output.query_offset
                            && let Err(error) = io.write(STARTUP_CURSOR_RESPONSE)
                        {
                            output.visible.splice(
                                query_offset..query_offset,
                                STARTUP_CURSOR_QUERY.iter().copied(),
                            );
                            tracing::warn!(
                                id = %reader_id,
                                %error,
                                "could not answer ConPTY's startup cursor query; retaining it as terminal output"
                            );
                        }
                        if !output.visible.is_empty() {
                            owned.ingest(&output.visible);
                        }
                    }
                    #[cfg(not(windows))]
                    owned.ingest(&chunk[..read]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                // A closed pty reports EIO on Linux rather than EOF, so any other
                // error is the end of output and not a condition to report.
                Err(_) => break,
            }
        }
        #[cfg(windows)]
        {
            let trailing = startup_cursor.finish();
            if !trailing.is_empty() {
                owned.ingest(&trailing);
            }
        }
        // After the loop, so the latch means "every chunk is queued". Both exits
        // above are definite ends of output, which is what makes the waiter's wait
        // terminate in the ordinary case rather than always paying `DRAIN_GRACE`.
        owned.drain.mark_drained();
    });
    if let Err(error) = thread {
        tracing::warn!(%id, %error, "could not start the pty reader thread; output will not be retained");
        // Nothing will ever drain, so release the latch rather than making every
        // exit on this session wait out the full grace for output that cannot come.
        shared.drain.mark_drained();
    }
}

fn spawn_waiter(
    id: &PtyId,
    shared: Arc<SessionShared>,
    io: Arc<SessionIo>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    on_exit: Arc<dyn ExitObserver>,
) {
    let name = format!("zuno-pty-wait-{id}");
    let owned = id.clone();
    let thread = std::thread::Builder::new().name(name).spawn(move || {
        // Blocking `wait` is also the reap: it is the only call that collects the
        // zombie, so this thread existing is what keeps a killed child from
        // lingering in the process table. It stays *ahead* of the drain wait below,
        // so a slow drain can never delay reaping.
        let exit_code = child.wait().ok().map(|status| status.exit_code());

        // ConPTY does not produce EOF merely because the child exited. Every
        // host-side writer and the master itself must be closed first; otherwise the
        // reader blocks forever and this waiter's drain ordering becomes a deadlock.
        // Writer-before-master also avoids keeping the console input side alive while
        // the backend is being closed.
        io.close();

        // The ordering contract (see the module docs): the child being dead does not
        // mean its output has been read. Hold `Ended` until the reader has drained
        // the pty, so no subscriber stops one chunk early.
        if !shared.drain.wait_for_drain(DRAIN_GRACE) {
            tracing::debug!(
                %owned,
                grace_ms = DRAIN_GRACE.as_millis(),
                "the pty stayed open after the child exited, most likely a grandchild \
                 inherited it; publishing the exit without waiting for more output"
            );
        }

        // Two phases, because they need different locks. `announce` runs inside the
        // session's state lock so no observer can see `Exited` before the event;
        // `record` takes the registry lock, which must never be held under it.
        if shared.mark_exited(exit_code, &|code| on_exit.announce(&owned, code)) {
            on_exit.record(&owned);
        }
    });
    if let Err(error) = thread {
        tracing::error!(%id, %error, "could not start the pty waiter thread; the child will not be reaped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complete_startup_cursor_query_is_answered_but_not_exposed() {
        let mut negotiation = StartupCursorNegotiation::default();

        let output = negotiation.push(b"before\x1b[6nafter");

        assert_eq!(
            output,
            StartupCursorOutput {
                visible: b"beforeafter".to_vec(),
                query_offset: Some(6),
            }
        );
        assert_eq!(
            negotiation.push(b"\x1b[6n"),
            StartupCursorOutput {
                visible: b"\x1b[6n".to_vec(),
                query_offset: None,
            },
            "only the inherited-cursor startup query is consumed"
        );
    }

    #[test]
    fn a_startup_cursor_query_may_span_reader_chunks() {
        let mut negotiation = StartupCursorNegotiation::default();

        assert_eq!(
            negotiation.push(b"prefix\x1b["),
            StartupCursorOutput {
                visible: b"prefix".to_vec(),
                query_offset: None,
            }
        );
        assert_eq!(
            negotiation.push(b"6nsuffix"),
            StartupCursorOutput {
                visible: b"suffix".to_vec(),
                query_offset: Some(0),
            }
        );
    }

    #[test]
    fn a_non_query_prefix_is_released_as_soon_as_it_cannot_match() {
        let mut negotiation = StartupCursorNegotiation::default();

        assert_eq!(
            negotiation.push(b"visible\x1b["),
            StartupCursorOutput {
                visible: b"visible".to_vec(),
                query_offset: None,
            }
        );
        assert_eq!(
            negotiation.push(b"5mred"),
            StartupCursorOutput {
                visible: b"\x1b[5mred".to_vec(),
                query_offset: None,
            }
        );
    }

    #[test]
    fn an_unfinished_query_prefix_is_visible_when_the_pty_closes() {
        let mut negotiation = StartupCursorNegotiation::default();

        assert_eq!(
            negotiation.push(b"before\x1b["),
            StartupCursorOutput {
                visible: b"before".to_vec(),
                query_offset: None,
            }
        );
        assert_eq!(negotiation.finish(), b"\x1b[");
    }

    #[test]
    fn minted_ids_carry_the_oracle_shape_and_strictly_ascend() {
        let ids: Vec<PtyId> = (0..64).map(|_| PtyId::mint()).collect();
        for id in &ids {
            let raw = id.as_str();
            let body = raw.strip_prefix("pty_").expect("every id is pty_-prefixed");
            assert_eq!(body.len(), 26, "12 hex + 14 base62, got {raw}");
            assert!(
                body[..12]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                "the timestamp must be lowercase hex: {raw}"
            );
            assert!(
                body[12..].bytes().all(|byte| byte.is_ascii_alphanumeric()),
                "the random tail must be base62: {raw}"
            );
        }
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            sorted, ids,
            "ids minted in one process must sort by creation"
        );
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ids must be unique");
    }

    #[test]
    fn info_serializes_with_the_wire_field_names() {
        let info = PtyInfo {
            id: PtyId::from_raw("pty_test"),
            title: "Terminal test".to_owned(),
            command: "/bin/sh".to_owned(),
            args: vec!["-l".to_owned()],
            cwd: "/tmp".to_owned(),
            status: PtyStatus::Exited,
            pid: 42,
            exit_code: Some(7),
        };
        let json = serde_json::to_value(&info).expect("PtyInfo serializes");
        assert_eq!(json["id"], "pty_test");
        assert_eq!(json["status"], "exited");
        assert_eq!(json["exitCode"], 7);

        let running = PtyInfo {
            status: PtyStatus::Running,
            exit_code: None,
            ..info
        };
        let json = serde_json::to_value(&running).expect("PtyInfo serializes");
        assert_eq!(json["status"], "running");
        assert!(
            json.get("exitCode").is_none(),
            "a running session must omit exitCode entirely"
        );
    }

    #[test]
    fn an_unlatched_drain_gate_gives_up_after_the_grace_rather_than_blocking() {
        let gate = DrainGate::default();
        let grace = Duration::from_millis(20);
        let started = std::time::Instant::now();
        let drained = gate.wait_for_drain(grace);
        let elapsed = started.elapsed();

        assert!(
            !drained,
            "an unlatched gate must report that it did not drain, so the caller can log it"
        );
        assert!(
            elapsed >= grace,
            "the wait returned in {elapsed:?}, before the {grace:?} grace; a spurious \
             wakeup must not be mistaken for a drain"
        );
    }

    #[test]
    fn an_already_latched_drain_gate_returns_without_waiting() {
        let gate = DrainGate::default();
        gate.mark_drained();
        // A ten-minute grace: returning `true` can only mean the latch was seen,
        // because timing out would hang the test rather than fail it. That makes the
        // assertion a fact about the gate rather than about the clock.
        assert!(gate.wait_for_drain(Duration::from_secs(600)));
    }

    #[test]
    fn latching_from_another_thread_wakes_the_waiter() {
        let gate = Arc::new(DrainGate::default());
        let latcher = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            latcher.mark_drained();
        });

        // Same ten-minute grace: if the notify were missing this would hang, so
        // `true` proves the wakeup rather than merely the flag.
        assert!(gate.wait_for_drain(Duration::from_secs(600)));
        handle.join().expect("the latching thread finished");
    }

    #[test]
    fn latching_twice_is_harmless() {
        let gate = DrainGate::default();
        gate.mark_drained();
        gate.mark_drained();
        assert!(gate.wait_for_drain(Duration::from_secs(600)));
    }

    #[test]
    fn a_zero_dimension_size_is_raised_rather_than_passed_to_the_kernel() {
        let size = PtySize::from(TerminalSize { rows: 0, cols: 0 });
        assert_eq!((size.rows, size.cols), (1, 1));
        assert_eq!(size.pixel_width, 0);
    }

    #[test]
    fn default_inputs_deserialize_from_an_empty_object() {
        let create: CreateInput =
            serde_json::from_str("{}").expect("CreateInput has all-optional fields");
        assert_eq!(create, CreateInput::default());
        let update: UpdateInput =
            serde_json::from_str("{}").expect("UpdateInput has all-optional fields");
        assert_eq!(update, UpdateInput::default());
        let sized: CreateInput =
            serde_json::from_str(r#"{"size":{"rows":40,"cols":120}}"#).expect("size deserializes");
        assert_eq!(
            sized.size,
            Some(TerminalSize {
                rows: 40,
                cols: 120
            })
        );
    }
}
