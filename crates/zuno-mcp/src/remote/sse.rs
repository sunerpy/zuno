use std::collections::VecDeque;

use zuno_llm::buffer::release_byte_capacity;
use zuno_llm::sse::{STREAM_MAX_EVENT_BYTES_ENV, StreamLimits};

#[derive(Debug)]
pub(super) struct SseEvent {
    pub(super) event: Option<String>,
    pub(super) data: String,
}

pub(super) struct SseDecoder {
    bytes: Vec<u8>,
    events: VecDeque<SseEvent>,
    max_event_bytes: usize,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            events: VecDeque::new(),
            // The same cap and the same `ZUNO_STREAM_MAX_EVENT_BYTES` override the
            // provider transports use. Two SSE decoders in one workspace disagreeing
            // about how large an event may be is how one of them ends up unbounded.
            max_event_bytes: StreamLimits::from_environment().max_event_bytes(),
        }
    }
}

impl SseDecoder {
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.bytes.extend_from_slice(bytes);
        // One chunk can carry hundreds of events: `reqwest` grows its read buffer
        // towards 512 KiB on a fast peer, and a legacy MCP server's pre-`endpoint`
        // backlog arrives as a few such reads. The frames are therefore cut against a
        // moving offset and the consumed prefix is drained once, after the last complete
        // frame. Draining per frame moved the whole remainder of the chunk for every
        // event in it, which with the rescan `frame_end` used to do made a chunk cost
        // quadratic in its size — on a debug build, over a second per 350 KiB chunk, so
        // a 1 MiB backlog outlived a 5 s request deadline on a slower host and the peer
        // was reported as a timeout instead of the protocol fault it was.
        let mut consumed = 0;
        let outcome = loop {
            let Some((end, delimiter)) = frame_end(&self.bytes[consumed..]) else {
                break Ok(());
            };
            if end > self.max_event_bytes {
                return Err(self.reject(end));
            }
            let frame = &self.bytes[consumed..consumed + end];
            consumed += end + delimiter;
            match std::str::from_utf8(frame) {
                Ok(frame) => {
                    if let Some(event) = parse_event(frame) {
                        self.events.push_back(event);
                    }
                }
                Err(error) => break Err(format!("SSE event was not UTF-8: {error}")),
            }
        };
        self.bytes.drain(..consumed);
        outcome?;
        // A server that never sends a blank line would otherwise grow `bytes` without
        // limit, which is the defect the perf plan §2 names.
        if self.bytes.len() > self.max_event_bytes {
            let stranded = self.bytes.len();
            return Err(self.reject(stranded));
        }
        release_byte_capacity(&mut self.bytes);
        Ok(())
    }

    /// Refuse the stream and drop what it accumulated, rather than truncate it.
    ///
    /// Truncation would hand the caller a syntactically broken JSON-RPC frame whose
    /// error names the wrong cause.
    fn reject(&mut self, actual_bytes: usize) -> String {
        self.bytes = Vec::new();
        format!(
            "MCP SSE event reached {actual_bytes} bytes, over the {} byte limit; \
             raise {} to accept a larger event",
            self.max_event_bytes, STREAM_MAX_EVENT_BYTES_ENV
        )
    }

    pub(super) fn pop(&mut self) -> Option<SseEvent> {
        self.events.pop_front()
    }

    pub(super) fn prepend(&mut self, mut events: VecDeque<SseEvent>) {
        events.append(&mut self.events);
        self.events = events;
    }
}

/// Where the first event separator starts, and how many bytes it spans.
///
/// The **earlier** of the two spellings wins, which is the whole reason this is not two
/// chained `or_else` lookups: both are legal in one stream, and searching for `\r\n\r\n`
/// unconditionally first skips an earlier `\n\n` and hands `parse_event` two events as
/// one frame. It then joins their `data:` payloads and the caller decodes the pair as a
/// single malformed JSON-RPC message. Matches `zuno_llm::sse`'s `next_separator`, which
/// compares the two positions for the same reason.
///
/// One forward pass over the line feeds decides both spellings at once and stops at the
/// first hit. Both separators contain a `\n`, and a `\r\n\r\n` starting at `i` is anchored
/// at `i + 1` while a `\n\n` starting at `i` is anchored at `i`; a `\n\n` cannot start
/// where a `\r\n\r\n` does, so the first anchor in byte order is also the earliest
/// separator. Searching each spelling to the end of the buffer separately cost a full
/// scan per frame — for the `\r\n\r\n` a stream never uses, every time — which is the
/// quadratic term [`SseDecoder::push`] describes.
fn frame_end(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(offset) = bytes[from..].iter().position(|&byte| byte == b'\n') {
        let anchor = from + offset;
        match bytes.get(anchor + 1) {
            Some(b'\n') => return Some((anchor, 2)),
            Some(b'\r')
                if anchor >= 1
                    && bytes[anchor - 1] == b'\r'
                    && bytes.get(anchor + 2) == Some(&b'\n') =>
            {
                return Some((anchor - 1, 4));
            }
            _ => {}
        }
        from = anchor + 1;
    }
    None
}

fn parse_event(frame: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data = Vec::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    (!data.is_empty()).then(|| SseEvent {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder(max_event_bytes: usize) -> SseDecoder {
        SseDecoder {
            bytes: Vec::new(),
            events: VecDeque::new(),
            max_event_bytes,
        }
    }

    #[test]
    fn a_stream_that_never_sends_a_blank_line_is_refused_at_the_cap() {
        const CAP: usize = 64 * 1024;
        // Four times the cap: enough headroom that a correct decoder is never cut off,
        // low enough that an unbounded one is caught before its per-push rescan of the
        // whole buffer turns the failure into a timeout instead of an assertion.
        const GIVE_UP_AFTER: usize = CAP * 4;
        let mut decoder = decoder(CAP);
        let mut delivered = 0;
        let chunk = vec![b'x'; 8 * 1024];
        let error = loop {
            delivered += chunk.len();
            match decoder.push(&chunk) {
                Ok(()) => assert!(
                    delivered <= GIVE_UP_AFTER,
                    "the decoder accepted {delivered} bytes with no delimiter and no \
                     refusal, against a {CAP}-byte cap, so it is unbounded"
                ),
                Err(error) => break error,
            }
        };
        assert!(
            error.contains("over the 65536 byte limit"),
            "the refusal must name the limit it enforced: {error}"
        );
        assert!(
            error.contains(STREAM_MAX_EVENT_BYTES_ENV),
            "the refusal must name how to raise the limit: {error}"
        );
        assert_eq!(
            decoder.bytes.capacity(),
            0,
            "the refused stream's bytes are still resident"
        );
    }

    #[test]
    fn a_large_event_does_not_strand_its_capacity_after_it_drains() {
        let mut decoder = decoder(8 * 1024 * 1024);
        let mut frame = b"data: ".to_vec();
        frame.extend(std::iter::repeat_n(b'y', 4 * 1024 * 1024));
        frame.extend_from_slice(b"\n\n");
        decoder
            .push(&frame)
            .expect("a 4 MiB event is under the cap");

        assert!(decoder.pop().is_some(), "the event itself must be parsed");
        assert!(
            decoder.bytes.capacity() <= zuno_llm::buffer::STEADY_STATE_CAPACITY_BYTES,
            "the drained decoder holds {} bytes of capacity",
            decoder.bytes.capacity()
        );
    }

    #[test]
    fn the_shipped_cap_is_the_one_the_provider_transports_use() {
        assert_eq!(
            SseDecoder::default().max_event_bytes,
            StreamLimits::from_environment().max_event_bytes()
        );
    }

    /// Every `data:` payload the decoder handed back, in order.
    fn drain(decoder: &mut SseDecoder) -> Vec<String> {
        std::iter::from_fn(|| decoder.pop())
            .map(|event| event.data)
            .collect()
    }

    #[test]
    fn an_lf_separated_event_is_not_merged_into_a_later_crlf_separated_one() {
        // Both separators are legal in the same stream — the wire format permits either
        // per line — and a decoder that looks for CRLFCRLF first skips the earlier LFLF
        // and returns both events as one frame. `parse_event` then joins the two `data:`
        // payloads with a newline, so the caller decodes `{"id":1}\n{"id":2}` as one
        // JSON-RPC message, fails, and errors out the *first* call as malformed.
        let mut decoder = decoder(64 * 1024);
        decoder
            .push(b"data: {\"id\":1}\n\ndata: {\"id\":2}\r\n\r\n")
            .expect("both events are far under the cap");

        assert_eq!(
            drain(&mut decoder),
            vec![String::from("{\"id\":1}"), String::from("{\"id\":2}")],
            "the LF separator came first, so it ends the first frame"
        );
    }

    #[test]
    fn a_crlf_separated_event_is_not_merged_into_a_later_lf_separated_one() {
        // The mirror case, which a fix that merely swapped the search order would break:
        // whichever separator appears *earlier* ends the frame, not whichever spelling
        // the decoder happens to try first.
        let mut decoder = decoder(64 * 1024);
        decoder
            .push(b"data: {\"id\":1}\r\n\r\ndata: {\"id\":2}\n\n")
            .expect("both events are far under the cap");

        assert_eq!(
            drain(&mut decoder),
            vec![String::from("{\"id\":1}"), String::from("{\"id\":2}")],
            "the CRLF separator came first, so it ends the first frame"
        );
    }

    /// A legacy MCP server's pre-`endpoint` backlog reaches the decoder as a few reads
    /// of up to 512 KiB, each carrying hundreds of small events. Framing one such chunk
    /// must cost time linear in its size: the previous decoder rescanned and moved the
    /// whole remainder once per event, and on a debug build a 1 MiB backlog then took
    /// longer than the 5 s request deadline on a Windows host, so the peer surfaced as
    /// a body timeout rather than the protocol fault its size was.
    ///
    /// Sized so the two shapes are not close: the quadratic decoder needs minutes for
    /// this chunk on a debug build, the linear one well under a second, and the bound
    /// sits between them with room for a loaded CI host.
    #[test]
    fn a_chunk_carrying_thousands_of_events_is_framed_in_linear_time() {
        const EVENTS: usize = 7_000;
        let event = format!("event: message\ndata: {}\n\n", "x".repeat(580));
        let chunk = event.repeat(EVENTS).into_bytes();
        assert!(
            chunk.len() > 4 * 1024 * 1024,
            "the chunk must dwarf a socket read"
        );
        let mut decoder = decoder(64 * 1024);

        let started = std::time::Instant::now();
        decoder
            .push(&chunk)
            .expect("every event is far under the cap");
        let elapsed = started.elapsed();

        assert_eq!(
            drain(&mut decoder).len(),
            EVENTS,
            "every event in the chunk must be framed"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "framing {} bytes took {elapsed:?}, which is the quadratic decoder",
            chunk.len()
        );
        assert!(
            decoder.bytes.is_empty(),
            "a chunk that ends on a separator leaves nothing pending"
        );
    }

    #[test]
    fn a_separator_split_across_two_chunks_still_ends_its_frame() {
        // A separator is four bytes and a socket read is whatever the kernel had, so the
        // split is routine rather than adversarial: the decoder must hold the partial
        // separator and complete the frame when the rest of it arrives.
        let mut decoder = decoder(64 * 1024);
        decoder
            .push(b"data: {\"id\":1}\r\n")
            .expect("a partial separator is not an event");
        assert!(
            decoder.pop().is_none(),
            "half a separator must not end a frame"
        );
        decoder
            .push(b"\r\ndata: {\"id\":2}\n\n")
            .expect("the rest of the separator completes the first frame");

        assert_eq!(
            drain(&mut decoder),
            vec![String::from("{\"id\":1}"), String::from("{\"id\":2}")],
            "a separator split across a chunk boundary lost an event"
        );
    }
}
