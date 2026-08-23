use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;
use zuno_error::ProviderError;
use zuno_llm::buffer::release_byte_capacity;
use zuno_llm::event::{FinishReason, PromptAccounting, StreamEvent};
use zuno_llm::sse::Utf8StreamDecoder;

const PRELUDE_LEN: usize = 12;
const OVERHEAD_LEN: usize = 16;
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrcKind {
    Prelude,
    Message,
}

impl fmt::Display for CrcKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prelude => formatter.write_str("prelude"),
            Self::Message => formatter.write_str("message"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventStreamError {
    #[error(
        "EventStream frame at byte offset {offset} declares invalid total length {total_length}"
    )]
    InvalidTotalLength { offset: usize, total_length: usize },
    #[error(
        "EventStream frame at byte offset {offset} declares {headers_length} header bytes inside a {total_length}-byte frame"
    )]
    InvalidHeadersLength {
        offset: usize,
        total_length: usize,
        headers_length: usize,
    },
    #[error(
        "EventStream frame at byte offset {offset} is {total_length} bytes, above the {maximum}-byte safety limit"
    )]
    FrameTooLarge {
        offset: usize,
        total_length: usize,
        maximum: usize,
    },
    #[error(
        "EventStream {kind} CRC mismatch at byte offset {offset}: expected {expected:#010x}, calculated {actual:#010x}"
    )]
    CrcMismatch {
        kind: CrcKind,
        offset: usize,
        expected: u32,
        actual: u32,
    },
    #[error(
        "truncated EventStream frame at byte offset {offset}: buffered {buffered} bytes, need {needed}"
    )]
    Truncated {
        offset: usize,
        buffered: usize,
        needed: usize,
    },
    #[error("invalid EventStream header at byte offset {offset}: {detail}")]
    InvalidHeader { offset: usize, detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderValue {
    Bool(bool),
    Byte(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Bytes(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

impl HeaderValue {
    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Bool(_)
            | Self::Byte(_)
            | Self::Int16(_)
            | Self::Int32(_)
            | Self::Int64(_)
            | Self::Bytes(_)
            | Self::Timestamp(_)
            | Self::Uuid(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventStreamMessage {
    pub offset: usize,
    pub headers: BTreeMap<String, HeaderValue>,
    pub payload: Vec<u8>,
}

impl EventStreamMessage {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.get(name)
    }

    #[must_use]
    pub fn string_header(&self, name: &str) -> Option<&str> {
        self.header(name).and_then(HeaderValue::as_str)
    }
}

#[derive(Debug, Default)]
pub struct EventStreamDecoder {
    buffer: Vec<u8>,
    stream_offset: usize,
}

impl EventStreamDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<EventStreamMessage>, EventStreamError> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();
        loop {
            if self.buffer.len() < PRELUDE_LEN {
                break;
            }
            let total_length = read_u32(&self.buffer[0..4]) as usize;
            let headers_length = read_u32(&self.buffer[4..8]) as usize;
            validate_lengths(self.stream_offset, total_length, headers_length)?;

            let expected_prelude_crc = read_u32(&self.buffer[8..12]);
            let actual_prelude_crc = crc32(&self.buffer[..8]);
            if expected_prelude_crc != actual_prelude_crc {
                return Err(EventStreamError::CrcMismatch {
                    kind: CrcKind::Prelude,
                    offset: self.stream_offset + 8,
                    expected: expected_prelude_crc,
                    actual: actual_prelude_crc,
                });
            }
            if self.buffer.len() < total_length {
                break;
            }

            let frame = &self.buffer[..total_length];
            let message_crc_offset = total_length - 4;
            let expected_message_crc = read_u32(&frame[message_crc_offset..]);
            let actual_message_crc = crc32(&frame[..message_crc_offset]);
            if expected_message_crc != actual_message_crc {
                return Err(EventStreamError::CrcMismatch {
                    kind: CrcKind::Message,
                    offset: self.stream_offset + message_crc_offset,
                    expected: expected_message_crc,
                    actual: actual_message_crc,
                });
            }

            let header_end = PRELUDE_LEN + headers_length;
            let headers = parse_headers(
                &frame[PRELUDE_LEN..header_end],
                self.stream_offset + PRELUDE_LEN,
            )?;
            messages.push(EventStreamMessage {
                offset: self.stream_offset,
                headers,
                payload: frame[header_end..message_crc_offset].to_vec(),
            });
            self.buffer.drain(..total_length);
            self.stream_offset += total_length;
        }
        // `drain` keeps the allocation, so one `MAX_FRAME_LEN`-sized frame would
        // otherwise pin 16 MiB resident for the rest of the stream. See
        // `zuno_llm::buffer`.
        release_byte_capacity(&mut self.buffer);
        Ok(messages)
    }

    pub fn finish(&mut self) -> Result<Vec<EventStreamMessage>, EventStreamError> {
        let messages = self.push(&[])?;
        if self.buffer.is_empty() {
            return Ok(messages);
        }
        let needed = if self.buffer.len() < PRELUDE_LEN {
            PRELUDE_LEN
        } else {
            read_u32(&self.buffer[..4]) as usize
        };
        Err(EventStreamError::Truncated {
            offset: self.stream_offset,
            buffered: self.buffer.len(),
            needed,
        })
    }
}

fn validate_lengths(
    offset: usize,
    total_length: usize,
    headers_length: usize,
) -> Result<(), EventStreamError> {
    if total_length < OVERHEAD_LEN {
        return Err(EventStreamError::InvalidTotalLength {
            offset,
            total_length,
        });
    }
    if total_length > MAX_FRAME_LEN {
        return Err(EventStreamError::FrameTooLarge {
            offset,
            total_length,
            maximum: MAX_FRAME_LEN,
        });
    }
    if headers_length > total_length - OVERHEAD_LEN {
        return Err(EventStreamError::InvalidHeadersLength {
            offset,
            total_length,
            headers_length,
        });
    }
    Ok(())
}

fn parse_headers(
    encoded: &[u8],
    absolute_offset: usize,
) -> Result<BTreeMap<String, HeaderValue>, EventStreamError> {
    let mut headers = BTreeMap::new();
    let mut cursor = 0usize;
    while cursor < encoded.len() {
        let header_offset = absolute_offset + cursor;
        let name_length = take(encoded, &mut cursor, 1, header_offset)?[0] as usize;
        let name_bytes = take(encoded, &mut cursor, name_length, header_offset)?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|error| EventStreamError::InvalidHeader {
                offset: header_offset,
                detail: format!("header name is not UTF-8: {error}"),
            })?
            .to_owned();
        let value_type = take(encoded, &mut cursor, 1, header_offset)?[0];
        let value = match value_type {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Byte(take(encoded, &mut cursor, 1, header_offset)?[0] as i8),
            3 => HeaderValue::Int16(i16::from_be_bytes(
                take(encoded, &mut cursor, 2, header_offset)?
                    .try_into()
                    .expect("slice length checked"),
            )),
            4 => HeaderValue::Int32(i32::from_be_bytes(
                take(encoded, &mut cursor, 4, header_offset)?
                    .try_into()
                    .expect("slice length checked"),
            )),
            5 => HeaderValue::Int64(i64::from_be_bytes(
                take(encoded, &mut cursor, 8, header_offset)?
                    .try_into()
                    .expect("slice length checked"),
            )),
            6 => {
                let length = read_header_length(encoded, &mut cursor, header_offset)?;
                HeaderValue::Bytes(take(encoded, &mut cursor, length, header_offset)?.to_vec())
            }
            7 => {
                let length = read_header_length(encoded, &mut cursor, header_offset)?;
                let bytes = take(encoded, &mut cursor, length, header_offset)?;
                let value = std::str::from_utf8(bytes).map_err(|error| {
                    EventStreamError::InvalidHeader {
                        offset: header_offset,
                        detail: format!("string value for `{name}` is not UTF-8: {error}"),
                    }
                })?;
                HeaderValue::String(value.to_owned())
            }
            8 => HeaderValue::Timestamp(i64::from_be_bytes(
                take(encoded, &mut cursor, 8, header_offset)?
                    .try_into()
                    .expect("slice length checked"),
            )),
            9 => HeaderValue::Uuid(
                take(encoded, &mut cursor, 16, header_offset)?
                    .try_into()
                    .expect("slice length checked"),
            ),
            other => {
                return Err(EventStreamError::InvalidHeader {
                    offset: header_offset,
                    detail: format!("header `{name}` uses unknown value type {other}"),
                });
            }
        };
        if headers.insert(name.clone(), value).is_some() {
            return Err(EventStreamError::InvalidHeader {
                offset: header_offset,
                detail: format!("header `{name}` appears more than once"),
            });
        }
    }
    Ok(headers)
}

fn read_header_length(
    encoded: &[u8],
    cursor: &mut usize,
    offset: usize,
) -> Result<usize, EventStreamError> {
    Ok(u16::from_be_bytes(
        take(encoded, cursor, 2, offset)?
            .try_into()
            .expect("slice length checked"),
    ) as usize)
}

fn take<'a>(
    encoded: &'a [u8],
    cursor: &mut usize,
    length: usize,
    offset: usize,
) -> Result<&'a [u8], EventStreamError> {
    let end = cursor.saturating_add(length);
    let value = encoded
        .get(*cursor..end)
        .ok_or_else(|| EventStreamError::InvalidHeader {
            offset,
            detail: format!(
                "value needs {length} bytes at header-relative offset {}, only {} remain",
                *cursor,
                encoded.len().saturating_sub(*cursor)
            ),
        })?;
    *cursor = end;
    Ok(value)
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("caller provides four bytes"))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[derive(Debug, thiserror::Error)]
pub enum BedrockDecodeError {
    #[error(transparent)]
    Framing(#[from] EventStreamError),
    #[error(transparent)]
    Payload(#[from] BedrockPayloadError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

#[derive(Debug, thiserror::Error)]
#[error("invalid Bedrock `{event_type}` payload at byte offset {offset}: {detail}")]
pub struct BedrockPayloadError {
    pub offset: usize,
    pub event_type: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BlockKind {
    Text,
    Tool { id: String },
    Reasoning,
}

#[derive(Debug, Default)]
pub struct BedrockEventDecoder {
    frames: EventStreamDecoder,
    payload_utf8: Utf8StreamDecoder,
    payload_buffer: String,
    pending_event: Option<(String, usize)>,
    inner_utf8: Utf8StreamDecoder,
    inner_buffer: String,
    blocks: BTreeMap<u64, BlockKind>,
    queued: VecDeque<StreamEvent>,
}

impl BedrockEventDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, BedrockDecodeError> {
        for frame in self.frames.push(chunk)? {
            self.decode_frame(frame)?;
        }
        Ok(self.queued.drain(..).collect())
    }

    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, BedrockDecodeError> {
        for frame in self.frames.finish()? {
            self.decode_frame(frame)?;
        }
        let trailing = self.payload_utf8.finish();
        if !trailing.is_empty() {
            self.payload_buffer.push_str(&trailing);
        }
        if !self.payload_buffer.is_empty() {
            let (event_type, offset) = self
                .pending_event
                .take()
                .unwrap_or_else(|| ("unknown".to_owned(), 0));
            return Err(
                payload_error(offset, &event_type, "stream ended inside a JSON payload").into(),
            );
        }
        let inner_trailing = self.inner_utf8.finish();
        if !inner_trailing.is_empty() || !self.inner_buffer.is_empty() {
            return Err(payload_error(
                0,
                "chunk",
                "stream ended inside an invoke-model JSON payload",
            )
            .into());
        }
        Ok(self.queued.drain(..).collect())
    }

    fn decode_frame(&mut self, frame: EventStreamMessage) -> Result<(), BedrockDecodeError> {
        let event_type = frame
            .string_header(":event-type")
            .or_else(|| frame.string_header(":exception-type"))
            .unwrap_or("unknown")
            .to_owned();
        if self.payload_buffer.is_empty() {
            self.pending_event = Some((event_type.clone(), frame.offset));
        }
        self.payload_buffer
            .push_str(&self.payload_utf8.decode(&frame.payload));
        if self.payload_buffer.is_empty() && self.payload_utf8.has_pending_bytes() {
            return Ok(());
        }

        let value = match serde_json::from_str::<Value>(&self.payload_buffer) {
            Ok(value) => value,
            Err(error) if error.is_eof() => return Ok(()),
            Err(error) => {
                return Err(payload_error(frame.offset, &event_type, error.to_string()).into());
            }
        };
        self.payload_buffer.clear();
        self.pending_event = None;

        if event_type == "chunk" {
            self.decode_invoke_chunk(frame.offset, &value)?;
            return Ok(());
        }
        if is_exception_event(&event_type, frame.string_header(":message-type")) {
            return Err(classify_stream_exception(&event_type, &value).into());
        }
        self.decode_converse_event(&event_type, &value);
        Ok(())
    }

    fn decode_invoke_chunk(
        &mut self,
        offset: usize,
        envelope: &Value,
    ) -> Result<(), BedrockDecodeError> {
        let encoded = envelope
            .get("bytes")
            .and_then(Value::as_str)
            .ok_or_else(|| payload_error(offset, "chunk", "missing string field `bytes`"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map_err(|error| payload_error(offset, "chunk", error.to_string()))?;
        self.inner_buffer.push_str(&self.inner_utf8.decode(&bytes));
        let value = match serde_json::from_str::<Value>(&self.inner_buffer) {
            Ok(value) => value,
            Err(error) if error.is_eof() => return Ok(()),
            Err(error) => {
                return Err(payload_error(offset, "chunk", error.to_string()).into());
            }
        };
        self.inner_buffer.clear();
        self.decode_native_event(&value);
        Ok(())
    }

    fn decode_converse_event(&mut self, event_type: &str, value: &Value) {
        match event_type {
            "messageStart" => {}
            "contentBlockStart" => self.content_block_start(value),
            "contentBlockDelta" => self.content_block_delta(value),
            "contentBlockStop" => self.content_block_stop(value),
            "messageStop" => self.message_stop(value),
            "metadata" => self.metadata(value),
            _ => {}
        }
    }

    fn content_block_start(&mut self, value: &Value) {
        let index = value
            .get("contentBlockIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if let Some(tool) = value.pointer("/start/toolUse") {
            let id = tool
                .get("toolUseId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
            self.blocks
                .insert(index, BlockKind::Tool { id: id.to_owned() });
            self.queued.push_back(StreamEvent::ToolUseStart {
                id: id.to_owned(),
                name: name.to_owned(),
            });
        } else if value.pointer("/start/reasoningContent").is_some() {
            self.blocks.insert(index, BlockKind::Reasoning);
            self.queued.push_back(StreamEvent::ReasoningStart);
        } else {
            self.blocks.insert(index, BlockKind::Text);
        }
    }

    fn content_block_delta(&mut self, value: &Value) {
        let index = value
            .get("contentBlockIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if let Some(text) = value.pointer("/delta/text").and_then(Value::as_str) {
            self.blocks.entry(index).or_insert(BlockKind::Text);
            if !text.is_empty() {
                self.queued
                    .push_back(StreamEvent::TextDelta(text.to_owned()));
            }
        }
        if let Some(input) = value
            .pointer("/delta/toolUse/input")
            .and_then(Value::as_str)
            && !input.is_empty()
        {
            let id = match self.blocks.get(&index) {
                Some(BlockKind::Tool { id }) => id.clone(),
                _ => String::new(),
            };
            self.queued.push_back(StreamEvent::ToolInputDelta {
                id,
                delta: input.to_owned(),
            });
        }
        if let Some(text) = value
            .pointer("/delta/reasoningContent/text")
            .and_then(Value::as_str)
        {
            if self.blocks.insert(index, BlockKind::Reasoning) != Some(BlockKind::Reasoning) {
                self.queued.push_back(StreamEvent::ReasoningStart);
            }
            if !text.is_empty() {
                self.queued
                    .push_back(StreamEvent::ReasoningDelta(text.to_owned()));
            }
        }
        if let Some(signature) = value
            .pointer("/delta/reasoningContent/signature")
            .and_then(Value::as_str)
            && !signature.is_empty()
        {
            self.queued
                .push_back(StreamEvent::ReasoningSignatureDelta(signature.to_owned()));
        }
    }

    fn content_block_stop(&mut self, value: &Value) {
        let index = value
            .get("contentBlockIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        match self.blocks.remove(&index) {
            Some(BlockKind::Tool { id }) => {
                self.queued.push_back(StreamEvent::ToolUseEnd { id });
            }
            Some(BlockKind::Reasoning) => self.queued.push_back(StreamEvent::ReasoningEnd),
            Some(BlockKind::Text) | None => {}
        }
    }

    fn message_stop(&mut self, value: &Value) {
        let reason = value
            .get("stopReason")
            .and_then(Value::as_str)
            .map(finish_reason);
        self.queued.push_back(StreamEvent::MessageEnd {
            stop_reason: reason,
        });
    }

    fn metadata(&mut self, value: &Value) {
        let usage = value.get("usage").unwrap_or(&Value::Null);
        self.queued.push_back(StreamEvent::TokenUsage {
            input_tokens: usage.get("inputTokens").and_then(Value::as_u64),
            output_tokens: usage.get("outputTokens").and_then(Value::as_u64),
            cache_read_input_tokens: usage.get("cacheReadInputTokens").and_then(Value::as_u64),
            cache_write_input_tokens: usage.get("cacheWriteInputTokens").and_then(Value::as_u64),
            // Converse defines `totalTokens` as `inputTokens + outputTokens` with no
            // cache term, so the cache figures itemise `inputTokens`.
            accounting: PromptAccounting::CacheInsideInput,
        });
    }

    fn decode_native_event(&mut self, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = value.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        self.blocks
                            .insert(index, BlockKind::Tool { id: id.clone() });
                        self.queued.push_back(StreamEvent::ToolUseStart {
                            id,
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        });
                    }
                    Some("thinking") => {
                        self.blocks.insert(index, BlockKind::Reasoning);
                        self.queued.push_back(StreamEvent::ReasoningStart);
                    }
                    _ => {
                        self.blocks.insert(index, BlockKind::Text);
                    }
                }
            }
            Some("content_block_delta") => self.native_delta(value),
            Some("content_block_stop") => self.content_block_stop(value),
            Some("message_delta") => {
                if let Some(usage) = value.get("usage") {
                    self.queued.push_back(StreamEvent::TokenUsage {
                        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
                        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
                        cache_read_input_tokens: usage
                            .get("cache_read_input_tokens")
                            .and_then(Value::as_u64),
                        cache_write_input_tokens: usage
                            .get("cache_creation_input_tokens")
                            .and_then(Value::as_u64),
                        // `InvokeModelWithResponseStream` passes Anthropic's own Messages
                        // payload through, and its three prompt figures are disjoint —
                        // unlike Converse's above.
                        accounting: PromptAccounting::CacheBesideInput,
                    });
                }
                let stop_reason = value
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .map(finish_reason);
                if stop_reason.is_some() {
                    self.queued
                        .push_back(StreamEvent::MessageEnd { stop_reason });
                }
            }
            _ => {}
        }
    }

    fn native_delta(&mut self, value: &Value) {
        let delta = value.get("delta").unwrap_or(&Value::Null);
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => push_nonempty(
                &mut self.queued,
                delta.get("text").and_then(Value::as_str),
                StreamEvent::TextDelta,
            ),
            Some("input_json_delta") => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(fragment) = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .filter(|fragment| !fragment.is_empty())
                {
                    let id = match self.blocks.get(&index) {
                        Some(BlockKind::Tool { id }) => id.clone(),
                        _ => String::new(),
                    };
                    self.queued.push_back(StreamEvent::ToolInputDelta {
                        id,
                        delta: fragment.to_owned(),
                    });
                }
            }
            Some("thinking_delta") => push_nonempty(
                &mut self.queued,
                delta.get("thinking").and_then(Value::as_str),
                StreamEvent::ReasoningDelta,
            ),
            Some("signature_delta") => push_nonempty(
                &mut self.queued,
                delta.get("signature").and_then(Value::as_str),
                StreamEvent::ReasoningSignatureDelta,
            ),
            _ => {}
        }
    }
}

fn push_nonempty(
    queue: &mut VecDeque<StreamEvent>,
    value: Option<&str>,
    event: impl FnOnce(String) -> StreamEvent,
) {
    if let Some(value) = value
        && !value.is_empty()
    {
        queue.push_back(event(value.to_owned()));
    }
}

fn finish_reason(value: &str) -> FinishReason {
    match value {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "content_filtered" | "guardrail_intervened" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

fn is_exception_event(event_type: &str, message_type: Option<&str>) -> bool {
    message_type == Some("exception")
        || event_type.ends_with("Exception")
        || event_type.ends_with("exception")
}

fn classify_stream_exception(event_type: &str, body: &Value) -> ProviderError {
    let retry_after = body
        .get("retryAfterSeconds")
        .and_then(Value::as_u64)
        .map(Duration::from_secs);
    match event_type {
        "throttlingException" | "ThrottlingException" => ProviderError::RateLimited { retry_after },
        "accessDeniedException"
        | "AccessDeniedException"
        | "unrecognizedClientException"
        | "UnrecognizedClientException" => ProviderError::Auth {
            provider: "amazon-bedrock".to_owned(),
            source: None,
        },
        "serviceUnavailableException"
        | "ServiceUnavailableException"
        | "internalServerException"
        | "InternalServerException"
        | "modelNotReadyException"
        | "ModelNotReadyException" => ProviderError::Transient {
            status: body
                .get("originalStatusCode")
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok()),
            source: None,
        },
        "contextLengthExceededException" | "ContextLengthExceededException" => {
            ProviderError::ContextLimit {
                limit_tokens: body.get("maxInputTokens").and_then(Value::as_u64),
                used_tokens: body.get("inputTokenCount").and_then(Value::as_u64),
            }
        }
        "guardrailIntervened" | "GuardrailIntervened" => ProviderError::Refused {
            provider: "amazon-bedrock".to_owned(),
            provider_text: body
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        _ => {
            let status = body
                .get("originalStatusCode")
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok())
                .or(Some(400));
            if status.is_some_and(|status| status >= 500) {
                ProviderError::Transient {
                    status,
                    source: None,
                }
            } else {
                ProviderError::Fatal {
                    status,
                    source: None,
                }
            }
        }
    }
}

fn payload_error(
    offset: usize,
    event_type: &str,
    detail: impl Into<String>,
) -> BedrockPayloadError {
    BedrockPayloadError {
        offset,
        event_type: event_type.to_owned(),
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    /// One header-free frame carrying `payload`, with both CRCs computed.
    fn frame(payload: &[u8]) -> Vec<u8> {
        let total_length = u32::try_from(OVERHEAD_LEN + payload.len()).expect("frame fits in u32");
        let mut frame = Vec::with_capacity(total_length as usize);
        frame.extend_from_slice(&total_length.to_be_bytes());
        frame.extend_from_slice(&0_u32.to_be_bytes());
        let prelude_crc = crc32(&frame[..8]);
        frame.extend_from_slice(&prelude_crc.to_be_bytes());
        frame.extend_from_slice(payload);
        let message_crc = crc32(&frame);
        frame.extend_from_slice(&message_crc.to_be_bytes());
        frame
    }

    #[test]
    fn a_large_frame_does_not_strand_its_capacity_for_the_rest_of_the_stream() {
        let mut decoder = EventStreamDecoder::new();
        let messages = decoder
            .push(&frame(&vec![b'p'; 4 * 1024 * 1024]))
            .expect("a 4 MiB frame is under the 16 MiB cap");
        assert_eq!(messages.len(), 1, "the frame itself must still be decoded");
        assert_eq!(messages[0].payload.len(), 4 * 1024 * 1024);

        assert!(
            decoder.buffer.capacity() <= zuno_llm::buffer::STEADY_STATE_CAPACITY_BYTES,
            "the drained decoder holds {} bytes of capacity",
            decoder.buffer.capacity()
        );
    }

    #[test]
    fn ordinary_frames_after_a_large_one_never_reallocate() {
        let mut decoder = EventStreamDecoder::new();
        let _ = decoder
            .push(&frame(&vec![b'q'; 4 * 1024 * 1024]))
            .expect("the large frame decodes");
        let settled = decoder.buffer.capacity();

        for _ in 0..500 {
            let messages = decoder.push(&frame(b"{}")).expect("a small frame decodes");
            assert_eq!(messages.len(), 1);
        }

        assert_eq!(
            decoder.buffer.capacity(),
            settled,
            "steady-state decoding reallocated after the large frame"
        );
    }

    #[test]
    fn finish_reports_a_partial_prelude_at_its_absolute_offset() {
        let mut decoder = EventStreamDecoder::new();
        decoder.push(&[0, 0, 0]).expect("not enough for a prelude");
        assert_eq!(
            decoder.finish(),
            Err(EventStreamError::Truncated {
                offset: 0,
                buffered: 3,
                needed: PRELUDE_LEN,
            })
        );
    }
}
