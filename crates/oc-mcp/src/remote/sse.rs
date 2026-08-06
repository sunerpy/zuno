use std::collections::VecDeque;

#[derive(Debug)]
pub(super) struct SseEvent {
    pub(super) event: Option<String>,
    pub(super) data: String,
}

#[derive(Default)]
pub(super) struct SseDecoder {
    bytes: Vec<u8>,
    events: VecDeque<SseEvent>,
}

impl SseDecoder {
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.bytes.extend_from_slice(bytes);
        while let Some((end, delimiter)) = frame_end(&self.bytes) {
            let frame = self.bytes.drain(..end).collect::<Vec<_>>();
            self.bytes.drain(..delimiter);
            let frame = std::str::from_utf8(&frame)
                .map_err(|error| format!("SSE event was not UTF-8: {error}"))?;
            if let Some(event) = parse_event(frame) {
                self.events.push_back(event);
            }
        }
        Ok(())
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
