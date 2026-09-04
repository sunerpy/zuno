use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::stdio::Notification;

pub(crate) type PendingResult = Result<Value, ReaderFailure>;
pub(crate) type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<PendingResult>>>>;

/// How many consecutive undecodable frames end a stream that never spoke JSON-RPC.
///
/// A frame here is one stdout line or one SSE `message` event: whatever the transport
/// hands the JSON parser as a single message. A frame that does not parse carries no
/// JSON-RPC id, so no entry of the id-keyed [`Pending`] map can be attributed to it,
/// and the frames around it are still correctly delimited. Charging one stray line to
/// every in-flight call — which is what [`fail_pending`] does — reported unrelated
/// tool calls as permanently failed and then dropped their real responses as unknown
/// ids; a lost response around a side effect has to reach the caller as its deadline,
/// not as a definite failure.
///
/// A run of them on a stream that has **never** produced a decodable frame is a
/// different fact: the peer is not speaking JSON-RPC here at all — an HTTP-only
/// server was configured as stdio, or a proxy replaced the stream — and letting every
/// call wait out its own deadline would hide that behind a retryable timeout. So that
/// run ends the connection with the framing violation it is.
///
/// The qualifier is load-bearing, and [`ReaderState::note_undecodable`] enforces it: a
/// peer that has framed even one JSON-RPC message has proven it speaks the protocol,
/// and no quantity of later noise may take that connection down. Without it, a working
/// server that logs a non-JSON heartbeat line to stdout accumulates the count while the
/// client is idle — nothing but a decodable frame resets it — and loses its reader on
/// the 32nd heartbeat.
///
/// The bound is generous because accidental noise (a loader warning, a stray `print`,
/// a progress line) comes in ones and twos, while a stream that is not JSON-RPC never
/// produces a decodable frame to reset the count.
pub const MAX_CONSECUTIVE_UNDECODABLE_FRAMES: usize = 32;

/// How often a run past the bound is still logged at `warn`.
///
/// Only a stream that has already framed a JSON-RPC message can run past the bound
/// (see [`MAX_CONSECUTIVE_UNDECODABLE_FRAMES`]), and such a stream is never ended, so
/// the run is unbounded. One `warn` per frame would let a peer that writes junk in a
/// loop flood the log as fast as it can write; one per this many keeps the fact and
/// the growing count visible without handing the peer the log volume.
///
/// # What this interval is keyed to
///
/// One counter per connection, shared by every frame that was not a decodable
/// JSON-RPC message: a malformed line, a non-`message` payload, and a blank line. That
/// is deliberate — they are one fault from the caller's point of view, and the count
/// this throttle reports is the run that ends the stream. It is *not* keyed per cause,
/// so a blank line and a malformed line spend the same budget; what keeps the frequent
/// benign case from consuming the rare dangerous one is that a decodable frame resets
/// the run, and that a blank frame is only ever loud on the interval (see
/// [`ReaderState::note_undecodable`]).
const LOUD_UNDECODABLE_INTERVAL: usize = 1024;

/// Largest excerpt of an undecodable frame kept for a later diagnosis.
///
/// One frame may be as large as [`crate::stdio::MAX_FRAME_BYTES`], and
/// [`ReaderState`] outlives it: retaining the whole thing so a later request can name
/// it would hold 64 MiB for the life of the connection. The excerpt only ever reaches
/// a message, never a decision, and the diagnostic value of
/// `INFO: Uvicorn running on http://0.0.0.0:8000` is in its first bytes.
const MAX_UNDECODABLE_EXCERPT_BYTES: usize = 256;

/// Whether losing the response to `method` leaves an outcome this client cannot know.
///
/// Written as a read-only allow-list rather than a side-effecting deny-list on
/// purpose: an unrecognized method is treated as possibly side-effecting, because a
/// method this client does not know cannot be promised to be a read. The permissive
/// direction here is the one that reports *less* certainty, and the tools it covers
/// declare [`zuno_tool::ToolReplayPolicy::Never`], so an uncertain report asks for
/// authoritative state to be inspected instead of replaying the call.
pub(crate) fn may_have_side_effects(method: &str) -> bool {
    !matches!(
        method,
        "initialize"
            | "ping"
            | "completion/complete"
            | "prompts/get"
            | "prompts/list"
            | "resources/list"
            | "resources/read"
            | "resources/templates/list"
            | "tools/list"
    )
}

/// What a reader has learned about its stream, for the request path to report.
///
/// Shared as an `Arc` between the reader task and the transport's own state, because
/// the two facts a caller needs are only observable inside the loop and only
/// actionable outside it: whether this reader has stopped — after which no response
/// can ever arrive, so a request must fail immediately instead of waiting out its
/// deadline against a permanently deaf connection — and whether this stream has ever
/// framed a JSON-RPC message, which is what tells a chatty server apart from one that
/// is not speaking the protocol.
#[derive(Default)]
pub(crate) struct ReaderState {
    consecutive_undecodable: AtomicUsize,
    decoded_any: AtomicBool,
    last_undecodable: Mutex<Option<Arc<str>>>,
    exit: Mutex<Option<ReaderFailure>>,
}

/// What a run of undecodable frames proves so far.
pub(crate) struct UndecodableRun {
    /// Consecutive undecodable frames including this one.
    pub(crate) count: usize,
    /// The framing violation to end the stream with, when the run proves one.
    pub(crate) violation: Option<ReaderFailure>,
    /// Whether this frame is worth a `warn` rather than a `debug`.
    pub(crate) loud: bool,
}

impl ReaderState {
    /// Records a frame that parsed as JSON.
    ///
    /// This is the only thing that resets the run, and the only thing that makes the
    /// peer's protocol competence a settled fact.
    pub(crate) fn note_decoded(&self) {
        self.decoded_any.store(true, Ordering::SeqCst);
        self.consecutive_undecodable.store(0, Ordering::SeqCst);
    }

    /// Records a frame that did not parse, and reports what the run now proves.
    ///
    /// # An empty frame is one of these
    ///
    /// A blank line is not JSON-RPC either, and skipping it above this counter is what
    /// let a peer writing nothing but `\n` emit one unthrottled `warn` per line —
    /// measured at 1,430,839 warns in one second — while the run stayed at zero, so the
    /// request reported a bare retryable timeout that named nothing. Charging it to the
    /// run fixes both: the throttle applies, and a stream of nothing but blank lines
    /// reaches [`MAX_CONSECUTIVE_UNDECODABLE_FRAMES`] and ends instead of spinning until
    /// the deadline.
    ///
    /// It is charged to the run but *not* to the `warn` budget on its own: a blank frame
    /// carries no evidence, and a server that terminates every message with a spare
    /// newline resets the run on every frame, so per-frame loudness there would be a
    /// frequent benign event spending the log budget that exists for the rare dangerous
    /// one. So a blank frame is loud only on [`LOUD_UNDECODABLE_INTERVAL`]; the run it
    /// ends still reports itself, at `warn`, through
    /// [`UndecodableRun::violation`](UndecodableRun).
    pub(crate) fn note_undecodable(&self, frame: &str) -> UndecodableRun {
        let count = self
            .consecutive_undecodable
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        let excerpt: Arc<str> = Arc::from(excerpt(frame));
        *lock(&self.last_undecodable) = Some(Arc::clone(&excerpt));
        let never_decoded = !self.decoded_any.load(Ordering::SeqCst);
        let loud = if frame.is_empty() {
            count.is_multiple_of(LOUD_UNDECODABLE_INTERVAL)
        } else {
            count <= MAX_CONSECUTIVE_UNDECODABLE_FRAMES
                || count.is_multiple_of(LOUD_UNDECODABLE_INTERVAL)
        };
        UndecodableRun {
            count,
            violation: (never_decoded && count >= MAX_CONSECUTIVE_UNDECODABLE_FRAMES)
                .then_some(ReaderFailure::Undecodable { excerpt, count }),
            loud,
        }
    }

    /// The framing violation a stream that never framed JSON-RPC has committed.
    ///
    /// `None` once any frame has parsed, and `None` for a stream that has simply said
    /// nothing: silence is what a deadline is for, and reporting it as a framing
    /// violation would make a slow server permanently broken.
    ///
    /// A run whose last frame was empty reports with an empty excerpt, which
    /// [`ExchangeError::from`] renders as `NoJsonRpcFrames` rather than quoting `""` as
    /// the line the peer began. Blank output is still output, so it is a diagnosis and
    /// not silence.
    pub(crate) fn not_json_rpc(&self) -> Option<ReaderFailure> {
        if self.decoded_any.load(Ordering::SeqCst) {
            return None;
        }
        let count = self.consecutive_undecodable.load(Ordering::SeqCst);
        let excerpt = lock(&self.last_undecodable).clone()?;
        (count > 0).then_some(ReaderFailure::Undecodable { excerpt, count })
    }

    /// Records why the reader stopped. The first reason wins.
    pub(crate) fn note_exit(&self, failure: ReaderFailure) {
        let mut exit = lock(&self.exit);
        if exit.is_none() {
            *exit = Some(failure);
        }
    }

    /// Why the reader stopped, when it has.
    pub(crate) fn exit(&self) -> Option<ReaderFailure> {
        lock(&self.exit).clone()
    }
}

/// A prefix of `frame` no longer than [`MAX_UNDECODABLE_EXCERPT_BYTES`].
fn excerpt(frame: &str) -> &str {
    if frame.len() <= MAX_UNDECODABLE_EXCERPT_BYTES {
        return frame;
    }
    let mut end = MAX_UNDECODABLE_EXCERPT_BYTES;
    while end > 0 && !frame.is_char_boundary(end) {
        end -= 1;
    }
    &frame[..end]
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RpcResponseError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

impl std::fmt::Display for RpcResponseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "JSON-RPC error {}: {}", self.code, self.message)?;
        if let Some(data) = &self.data {
            write!(formatter, " ({data})")?;
        }
        Ok(())
    }
}

impl std::error::Error for RpcResponseError {}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExchangeError {
    #[error("MCP connection closed")]
    Closed,
    #[error("MCP request timed out")]
    Timeout,
    #[error("MCP request could not be encoded")]
    Encode(#[source] serde_json::Error),
    #[error("MCP response result could not be decoded")]
    DecodeResult(#[source] serde_json::Error),
    #[error(
        "MCP peer sent {count} consecutive frame(s) that were not JSON-RPC; the last began {excerpt:?}"
    )]
    NotJsonRpc { count: usize, excerpt: Arc<str> },
    /// The same violation, for a run whose last frame carried no bytes at all.
    ///
    /// Split from [`Self::NotJsonRpc`] rather than rendered as `the last began ""`
    /// because an empty excerpt is not an excerpt: quoting it names no line and reads
    /// as a client bug. A peer that writes only blank lines is the input that produced
    /// this variant.
    #[error("MCP peer sent {count} frame(s) that were not JSON-RPC; the last was empty")]
    NoJsonRpcFrames { count: usize },
    /// The stream ended under a call that may already have taken effect.
    ///
    /// Kept apart from every definite failure because it is the one thing this client
    /// can honestly say about a request it wrote and never got an answer to: the
    /// reader that would have delivered the answer is gone, and nothing about that
    /// says the server did not run the call. Maps to
    /// [`zuno_error::McpError::Timeout`], which is the class this crate has for an
    /// outcome that is unknown rather than failed.
    #[error("MCP stream ended while a call that may have taken effect was outstanding: {reason}")]
    Uncertain { reason: Arc<str> },
    #[error("MCP frame reached {bytes} bytes without a newline, past the {limit}-byte bound")]
    FrameTooLarge { bytes: usize, limit: usize },
    #[error("MCP stdin write failed")]
    Write(#[source] io::Error),
    #[error("MCP stdout read failed")]
    Read(#[source] io::Error),
    #[error(transparent)]
    Rpc(#[from] RpcResponseError),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone)]
pub(crate) enum ReaderFailure {
    Closed,
    Io {
        kind: io::ErrorKind,
        message: Arc<str>,
    },
    /// A run of frames the peer never framed as JSON-RPC.
    ///
    /// `count` is how many consecutive frames failed to parse and `excerpt` is a
    /// bounded prefix of the last of them: together they are the whole diagnosis for
    /// the misconfiguration this reports, which is a command that is not an MCP
    /// server. One undecodable frame never reaches here — see
    /// [`MAX_CONSECUTIVE_UNDECODABLE_FRAMES`].
    Undecodable {
        excerpt: Arc<str>,
        count: usize,
    },
    /// The peer wrote a frame past the byte bound without terminating it.
    ///
    /// Distinct from [`Self::Undecodable`], which has complete frames to show. Here the
    /// bytes read are a prefix of a value whose end was never announced, so there is
    /// no line worth reporting and no offset the reader may resume from.
    FrameTooLarge {
        bytes: usize,
        limit: usize,
    },
}

impl From<ReaderFailure> for ExchangeError {
    fn from(failure: ReaderFailure) -> Self {
        match failure {
            ReaderFailure::Closed => Self::Closed,
            ReaderFailure::Io { kind, message } => {
                Self::Read(io::Error::new(kind, message.to_string()))
            }
            ReaderFailure::Undecodable { excerpt, count } if excerpt.is_empty() => {
                Self::NoJsonRpcFrames { count }
            }
            ReaderFailure::Undecodable { excerpt, count } => Self::NotJsonRpc { count, excerpt },
            ReaderFailure::FrameTooLarge { bytes, limit } => Self::FrameTooLarge { bytes, limit },
        }
    }
}

pub(crate) fn decode_response(method: &str, message: Value) -> Result<Value, ExchangeError> {
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(ExchangeError::Invalid(format!(
            "MCP response for {method} did not use jsonrpc 2.0"
        )));
    }
    if let Some(error) = message.get("error") {
        let error = serde_json::from_value(error.clone()).map_err(ExchangeError::DecodeResult)?;
        return Err(ExchangeError::Rpc(error));
    }
    message.get("result").cloned().ok_or_else(|| {
        ExchangeError::Invalid(format!(
            "MCP response for {method} contained neither result nor error"
        ))
    })
}

pub(crate) fn route_message(
    server: &str,
    pending: &Pending,
    notifications: &broadcast::Sender<Notification>,
    refresh: &mpsc::Sender<()>,
    message: Value,
) {
    let Some(object) = message.as_object() else {
        tracing::warn!(%server, "MCP server emitted a non-object JSON-RPC message");
        return;
    };

    if let Some(method) = object.get("method").and_then(Value::as_str) {
        if let Some(id) = object.get("id") {
            tracing::warn!(%server, id = %id, %method, "unsupported MCP server request");
            return;
        }
        let notification = Notification {
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        };
        if method == "notifications/tools/list_changed" {
            let _result = refresh.try_send(());
        }
        let _receivers = notifications.send(notification);
        return;
    }

    let Some(id_value) = object.get("id") else {
        tracing::warn!(%server, "MCP server emitted a message with neither method nor id");
        return;
    };
    let Some(id) = id_value.as_u64() else {
        tracing::warn!(%server, id = %id_value, "MCP response id was not an unsigned integer");
        return;
    };
    let sender = lock(pending).remove(&id);
    match sender {
        Some(sender) => {
            let _receiver = sender.send(Ok(message));
        }
        None => tracing::warn!(%server, id, "MCP response id has no pending request"),
    }
}

/// Fail every in-flight request, returning how many there were.
///
/// The count is what lets a caller tell a server that died mid-call from one that
/// closed its stream with nothing outstanding. Both reach the same code path, but only
/// the first cost the user a tool call.
pub(crate) fn fail_pending(pending: &Pending, failure: ReaderFailure) -> usize {
    let waiters: Vec<_> = lock(pending).drain().map(|(_, waiter)| waiter).collect();
    let in_flight = waiters.len();
    for waiter in waiters {
        let _receiver = waiter.send(Err(failure.clone()));
    }
    in_flight
}

/// The framing violation an over-long frame is, expressed as a decode error.
///
/// [`zuno_error::McpError::Protocol`] is the variant this crate documents as the home
/// for framing bugs, and it contracts for a `serde_json::Error`. An unterminated frame
/// has no parse position to report, so this synthesizes the error from an
/// `InvalidData` I/O cause the same way [`not_json_rpc_error`] does for a run of frames
/// whose individual parse positions say nothing useful.
pub(crate) fn oversized_frame_error(bytes: usize, limit: usize) -> serde_json::Error {
    serde_json::Error::io(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "MCP frame reached {bytes} bytes without a newline, past the {limit}-byte frame bound"
        ),
    ))
}

/// The framing violation a stream that never spoke JSON-RPC is, as a decode error.
///
/// [`zuno_error::McpError::Protocol`] contracts for a `serde_json::Error`, so this
/// synthesizes one from an `InvalidData` I/O cause the way [`oversized_frame_error`]
/// does. It deliberately does **not** reparse the excerpt to recover a line and column:
/// this error reports a *run* of frames rather than one, and for the misconfiguration
/// it names — a command printing `INFO: Uvicorn running on ...` where JSON-RPC was
/// expected — the count and the text are the diagnosis, while a column number points
/// at a byte of prose.
pub(crate) fn not_json_rpc_error(count: usize, excerpt: &str) -> serde_json::Error {
    serde_json::Error::io(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "MCP peer sent {count} consecutive frame(s) that were not JSON-RPC; \
             the last began {excerpt:?}"
        ),
    ))
}

/// The same violation for a run of frames with no bytes to quote.
///
/// A peer that writes only blank lines has framed no JSON-RPC message either, and
/// before this the count was never taken: the line was skipped above the counter, so
/// the request reported a bare retryable deadline. Kept apart from
/// [`not_json_rpc_error`] because `the last began ""` names nothing.
pub(crate) fn no_json_rpc_frames_error(count: usize) -> serde_json::Error {
    serde_json::Error::io(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("MCP peer sent {count} frame(s) that were not JSON-RPC; the last was empty"),
    ))
}

/// A label for a reader failure that carries none of the peer's own bytes.
///
/// [`ExchangeError::NotJsonRpc`] renders a bounded excerpt of the peer's stream, so the
/// failure's `Display` may not go into a tracing field the redaction policy leaves
/// readable — `zuno_observability`'s predicate classifies field *names*, and a name it
/// does not know is printed verbatim into the plaintext log and the SQLite record. This
/// names the class instead, so the log stays diagnostic while the excerpt travels under
/// a field name that policy scrubs.
pub(crate) const fn reader_failure_label(failure: &ReaderFailure) -> &'static str {
    match failure {
        ReaderFailure::Closed => "stream-closed",
        ReaderFailure::Io { .. } => "read-failed",
        ReaderFailure::Undecodable { .. } => "not-json-rpc",
        ReaderFailure::FrameTooLarge { .. } => "frame-too-large",
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The predicate in isolation: proven competence outranks any amount of noise.
    #[test]
    fn a_run_past_the_bound_is_no_violation_once_a_frame_has_decoded() {
        let state = ReaderState::default();
        state.note_decoded();
        let mut last = None;
        for _ in 0..MAX_CONSECUTIVE_UNDECODABLE_FRAMES * 40 {
            last = Some(state.note_undecodable("[debug] still working"));
        }
        let run = last.expect("the loop ran");
        assert_eq!(run.count, MAX_CONSECUTIVE_UNDECODABLE_FRAMES * 40);
        assert!(
            run.violation.is_none(),
            "a stream that framed JSON-RPC may never be ended by noise"
        );
        assert!(
            state.not_json_rpc().is_none(),
            "and no later request may reclassify its deadline as a framing violation"
        );
    }

    /// And the other side: a stream that never framed one ends at the bound, with the
    /// count and the text that diagnose it.
    #[test]
    fn a_run_past_the_bound_ends_a_stream_that_never_decoded_anything() {
        let state = ReaderState::default();
        for index in 1..MAX_CONSECUTIVE_UNDECODABLE_FRAMES {
            let run = state.note_undecodable("INFO:     Uvicorn running on http://0.0.0.0:8000");
            assert_eq!(run.count, index);
            assert!(run.violation.is_none(), "under the bound at {index}");
        }
        let run = state.note_undecodable("INFO:     Uvicorn running on http://0.0.0.0:8000");
        let violation = run.violation.expect("the bound is reached");
        assert!(
            matches!(
                &violation,
                ReaderFailure::Undecodable { count, excerpt }
                    if *count == MAX_CONSECUTIVE_UNDECODABLE_FRAMES
                        && excerpt.contains("Uvicorn")
            ),
            "{violation:?}"
        );
    }

    /// The reviewer's input: a server that terminates every message with a blank line.
    ///
    /// Each blank line used to reach an unlatched `warn`, so a server emitting
    /// `\r\n\r\n` framing produced one log record per line — 1,430,839 of them over a
    /// long session, all of them saying nothing new. The interval is what bounds that,
    /// and it is shared with the malformed-line counter on purpose: a peer cannot
    /// alternate the two shapes to get 2x the records.
    #[test]
    fn blank_frames_are_loud_only_once_per_interval() {
        let state = ReaderState::default();
        // A decodable frame first, so the run is pure noise rather than a stream that
        // never spoke JSON-RPC; that case ends the connection instead of logging.
        state.note_decoded();
        let loud = (0..LOUD_UNDECODABLE_INTERVAL * 3)
            .filter(|_| state.note_undecodable("").loud)
            .count();
        assert_eq!(
            loud,
            3,
            "{} blank frames may not produce {loud} warn records",
            LOUD_UNDECODABLE_INTERVAL * 3
        );
        assert!(
            !state.note_undecodable("still not JSON").loud,
            "one counter per connection: a malformed line inside a blank-line run is \
             throttled by the same interval, so alternating the two shapes cannot double \
             the records"
        );
    }

    /// And a stream of nothing but blank lines is named without quoting an empty
    /// excerpt back at the user.
    ///
    /// A blank line is undecodable, so it counts toward the framing bound like any
    /// other non-JSON frame. Rendering that run through `NotJsonRpc` would produce
    /// `the last began ""`, which reads like a truncation bug rather than a diagnosis.
    #[test]
    fn a_run_of_blank_frames_ends_the_stream_and_names_itself() {
        let state = ReaderState::default();
        for index in 1..MAX_CONSECUTIVE_UNDECODABLE_FRAMES {
            assert!(
                state.note_undecodable("").violation.is_none(),
                "under the bound at {index}"
            );
        }
        let violation = state
            .note_undecodable("")
            .violation
            .expect("a stream that framed nothing but blank lines is not speaking JSON-RPC");
        assert!(
            matches!(
                ExchangeError::from(violation),
                ExchangeError::NoJsonRpcFrames { count }
                    if count == MAX_CONSECUTIVE_UNDECODABLE_FRAMES
            ),
            "a blank-line run must name itself, not quote an empty excerpt back"
        );
    }

    /// Silence is a deadline, not a framing violation, however long it lasts.
    #[test]
    fn a_stream_that_said_nothing_reports_no_framing_violation() {
        let state = ReaderState::default();
        assert!(state.not_json_rpc().is_none());
    }

    /// A single stray line is still enough to name, once something else — a deadline,
    /// or the peer closing — has already made the call fail.
    #[test]
    fn one_undecodable_frame_is_nameable_without_ending_the_stream() {
        let state = ReaderState::default();
        let run = state.note_undecodable("usage: some-cli [options]");
        assert!(run.violation.is_none());
        let named = state.not_json_rpc().expect("the frame is on the record");
        assert!(
            matches!(&named, ReaderFailure::Undecodable { count, excerpt }
                if *count == 1 && excerpt.starts_with("usage:")),
            "{named:?}"
        );
    }

    /// A 64 MiB frame may not be retained to describe a later failure.
    #[test]
    fn a_retained_excerpt_is_bounded_and_stays_on_a_character_boundary() {
        let state = ReaderState::default();
        let frame = "é".repeat(MAX_UNDECODABLE_EXCERPT_BYTES);
        let run = state.note_undecodable(&frame);
        let ReaderFailure::Undecodable { excerpt, .. } = state
            .not_json_rpc()
            .expect("an undecodable frame is on the record")
        else {
            panic!("the recorded failure is a framing violation")
        };
        assert!(run.violation.is_none());
        assert!(
            excerpt.len() <= MAX_UNDECODABLE_EXCERPT_BYTES,
            "kept {} bytes of a {}-byte frame",
            excerpt.len(),
            frame.len()
        );
        assert!(
            excerpt.chars().all(|character| character == 'é'),
            "a truncated excerpt must still be the text it came from: {excerpt:?}"
        );
    }

    /// The first exit reason is the one that explains the reader, not the last.
    #[test]
    fn the_first_recorded_exit_reason_wins() {
        let state = ReaderState::default();
        assert!(state.exit().is_none(), "a live reader has not exited");
        state.note_exit(ReaderFailure::Undecodable {
            excerpt: Arc::from("usage: some-cli"),
            count: MAX_CONSECUTIVE_UNDECODABLE_FRAMES,
        });
        state.note_exit(ReaderFailure::Closed);
        assert!(
            matches!(state.exit(), Some(ReaderFailure::Undecodable { .. })),
            "a close after a framing violation is a consequence, not the cause"
        );
    }

    /// The uncertainty classifier: reads are definite, everything else is not.
    #[test]
    fn losing_a_response_is_uncertain_for_every_method_that_is_not_a_known_read() {
        for method in [
            "initialize",
            "ping",
            "completion/complete",
            "prompts/get",
            "prompts/list",
            "resources/list",
            "resources/read",
            "resources/templates/list",
            "tools/list",
        ] {
            assert!(
                !may_have_side_effects(method),
                "{method} is a read and its loss is a definite failure"
            );
        }
        for method in ["tools/call", "logging/setLevel", "roots/list", "sampling/x"] {
            assert!(
                may_have_side_effects(method),
                "{method} may have taken effect and must not be reported as failed"
            );
        }
    }
}
