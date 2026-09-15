use serde_json::Value;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use super::client::Outbound;

pub(super) enum FrameRead {
    Eof,
    Frame(Vec<u8>),
    Oversized,
}

pub(super) async fn read_frame<R>(reader: &mut R, limit: usize) -> Result<FrameRead, std::io::Error>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if oversized {
                FrameRead::Oversized
            } else if frame.is_empty() {
                FrameRead::Eof
            } else {
                FrameRead::Frame(frame)
            });
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if !oversized {
                if frame.len().saturating_add(newline) > limit {
                    oversized = true;
                } else {
                    frame.extend_from_slice(&available[..newline]);
                }
            }
            reader.consume(newline + 1);
            return Ok(if oversized {
                FrameRead::Oversized
            } else {
                FrameRead::Frame(frame)
            });
        }

        let available_len = available.len();
        if !oversized {
            if frame.len().saturating_add(available_len) > limit {
                oversized = true;
            } else {
                frame.extend_from_slice(available);
            }
        }
        reader.consume(available_len);
    }
}

pub(super) async fn write_frames<W>(
    mut output: W,
    mut frames: mpsc::Receiver<Outbound>,
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = frames.recv().await {
        match frame {
            Outbound::Frame { value, sent } => {
                let result = async {
                    let mut encoded = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
                    encoded.push(b'\n');
                    output.write_all(&encoded).await?;
                    output.flush().await
                }
                .await;
                let completion = result.as_ref().map(|_| ()).map_err(ToString::to_string);
                let _ignored = sent.send(completion);
                result?;
            }
            Outbound::Close { closed } => {
                let _ignored = closed.send(());
                break;
            }
        }
    }
    Ok(())
}

pub(super) fn id_key(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".to_owned()),
        Value::String(value) => Some(format!("s:{value}")),
        Value::Number(value) if value.as_i64().is_some() || value.as_u64().is_some() => {
            Some(format!("n:{value}"))
        }
        Value::Bool(_) | Value::Array(_) | Value::Object(_) | Value::Number(_) => None,
    }
}
