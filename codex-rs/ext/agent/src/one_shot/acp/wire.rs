use super::super::claude_code::wait_for_deadline;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(super) struct AcpWire {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    max_message_bytes: usize,
}

impl AcpWire {
    pub(super) fn new(stdin: ChildStdin, stdout: ChildStdout, max_message_bytes: usize) -> Self {
        Self {
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            max_message_bytes,
        }
    }

    pub(super) async fn request(
        &mut self,
        method: &str,
        params: Value,
        deadline: Option<Instant>,
        cancellation: &CancellationToken,
        answer: &mut String,
    ) -> Result<Value, WireError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        loop {
            let frame = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(WireError::Cancelled),
                _ = wait_for_deadline(deadline) => return Err(WireError::Timeout),
                frame = read_bounded_json_line(&mut self.stdout, self.max_message_bytes) => frame?,
            };
            if frame.get("id").and_then(Value::as_u64) == Some(id) {
                if frame.get("error").is_some() {
                    return Err(WireError::Remote);
                }
                return frame.get("result").cloned().ok_or(WireError::Invalid);
            }
            if frame.get("method").and_then(Value::as_str) == Some("session/update") {
                append_agent_update(&frame, answer);
                continue;
            }
            if frame.get("id").is_some() && frame.get("method").is_some() {
                let response_id = frame.get("id").cloned().ok_or(WireError::Invalid)?;
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": response_id,
                    "error": {
                        "code": -32601,
                        "message": "non-interactive ACP backend cannot service client requests",
                    },
                }))
                .await?;
            }
        }
    }

    pub(super) async fn notify(&mut self, method: &str, params: Value) -> Result<(), WireError> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn send(&mut self, value: Value) -> Result<(), WireError> {
        let mut bytes = serde_json::to_vec(&value).map_err(|_| WireError::Invalid)?;
        if bytes.len() > self.max_message_bytes {
            return Err(WireError::Limit);
        }
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|_| WireError::Io)?;
        self.stdin.flush().await.map_err(|_| WireError::Io)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WireError {
    Io,
    Closed,
    Invalid,
    Remote,
    Limit,
    Cancelled,
    Timeout,
}

async fn read_bounded_json_line<R>(reader: &mut R, limit: usize) -> Result<Value, WireError>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|_| WireError::Io)?;
        if available.is_empty() {
            return Err(WireError::Closed);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(available.len());
        if bytes.len().saturating_add(take) > limit {
            return Err(WireError::Limit);
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take + usize::from(newline.is_some()));
        if newline.is_some() {
            if bytes.is_empty() {
                continue;
            }
            return serde_json::from_slice(&bytes).map_err(|_| WireError::Invalid);
        }
    }
}

fn append_agent_update(frame: &Value, answer: &mut String) {
    let update = frame.pointer("/params/update").unwrap_or(&Value::Null);
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("agent_message_chunk") {
        return;
    }
    if let Some(text) = update
        .pointer("/content/text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        answer.push_str(text);
    }
}
