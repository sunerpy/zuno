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
        while let Some((end, delimiter)) = frame_end(&self.bytes) {
            if end > self.max_event_bytes {
                return Err(self.reject(end));
            }
            let frame = self.bytes.drain(..end).collect::<Vec<_>>();
            self.bytes.drain(..delimiter);
            let frame = std::str::from_utf8(&frame)
                .map_err(|error| format!("SSE event was not UTF-8: {error}"))?;
            if let Some(event) = parse_event(frame) {
                self.events.push_back(event);
            }
        }
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

/// Where the first event separator ends, and how many bytes it spans.
///
/// The **earlier** of the two spellings wins, which is the whole reason this is not two
/// chained `or_else` lookups: both are legal in one stream, and searching for `\r\n\r\n`
/// unconditionally first skips an earlier `\n\n` and hands `parse_event` two events as
/// one frame. It then joins their `data:` payloads and the caller decodes the pair as a
/// single malformed JSON-RPC message. Matches `zuno_llm::sse`'s `next_separator`, which
/// compares the two positions for the same reason.
fn frame_end(bytes: &[u8]) -> Option<(usize, usize)> {
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(separator), None) | (None, Some(separator)) => Some(separator),
        (None, None) => None,
    }
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
