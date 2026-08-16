//! Reading a response body under a byte cap, while staying cancellable.
//!
//! # Why this is not `response.bytes().await`
//!
//! `bytes()` buffers the whole body before returning, so a 100 MB response is
//! 100 MB of resident memory before any cap can be consulted, and nothing can
//! cancel it. The cap has to be enforced *during* the read, and the interrupt has
//! to be polled between chunks, or "bounded" and "cancellable" are claims the code
//! does not actually make.
//!
//! # The two size checks are not redundant
//!
//! A declared `content-length` above the cap is rejected without opening the body at
//! all — the cheapest possible refusal. But `content-length` is absent on every
//! chunked response and is not trustworthy when present, so the streaming check is
//! the one that actually holds. Upstream does both for the same reason
//! (`packages/core/src/tool/http-body.ts:11-15` then `:20-22`).

use crate::webfetch::bounds::{INITIAL_BODY_CAPACITY, WebError};
use zuno_tool::InterruptHandle;

/// Reads `response`'s body, refusing to buffer more than `limit` bytes.
///
/// Returns [`WebError::TooLarge`] as soon as the cap would be exceeded — before the
/// offending chunk is retained, so peak memory stays at `limit` plus one chunk
/// regardless of how much the server intended to send.
///
/// `interrupt` is polled before every chunk, so a large download is abandoned
/// mid-stream rather than after it finishes.
///
/// # Errors
/// - [`WebError::TooLarge`] when `content-length` declares, or the stream delivers,
///   more than `limit` bytes.
/// - [`WebError::Interrupted`] when the interrupt fires while reading.
/// - [`WebError::Transport`] when the connection fails mid-body.
pub async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
    interrupt: &dyn InterruptHandle,
) -> Result<Vec<u8>, WebError> {
    let url = response.url().to_string();

    if let Some(declared) = declared_length(&response)
        && declared > limit
    {
        return Err(WebError::TooLarge { limit, read: 0 });
    }

    let capacity = declared_length(&response)
        .unwrap_or(INITIAL_BODY_CAPACITY)
        .min(limit);
    let mut body = Vec::with_capacity(capacity);

    loop {
        if interrupt.is_set() {
            return Err(WebError::Interrupted { read: body.len() });
        }

        let chunk = response
            .chunk()
            .await
            .map_err(|source| WebError::Transport {
                url: url.clone(),
                source,
            })?;

        let Some(chunk) = chunk else { break };
        if chunk.is_empty() {
            continue;
        }

        if body.len() + chunk.len() > limit {
            // Abandon before retaining the chunk: the point of the cap is that the
            // oversized bytes are never resident, not that they are dropped later.
            return Err(WebError::TooLarge {
                limit,
                read: body.len(),
            });
        }

        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// The `content-length` header, when it is present and a plausible size.
///
/// A negative, non-numeric or absurd value is treated as absent rather than as an
/// error, so a server with a broken header still gets its body read under the
/// streaming cap. Oracle: `packages/core/src/tool/http-body.ts:11-14` applies the
/// same `Number.isSafeInteger` and non-negative guards.
fn declared_length(response: &reqwest::Response) -> Option<usize> {
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|value| usize::try_from(value).ok())
}
