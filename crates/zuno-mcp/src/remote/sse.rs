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
        // limit, which is the defect `.omo/plans/memory-perf-optimization.md` §2 names.
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

fn frame_end(bytes: &[u8]) -> Option<(usize, usize)> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4))
        .or_else(|| {
            bytes
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| (index, 2))
        })
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
}
