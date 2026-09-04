//! Bounded reads of untrusted HTTP response bodies.
//!
//! Every HTTP body this crate reads comes from a peer named in user configuration
//! and is otherwise unvalidated, and several of them are reached at URLs the peer
//! itself supplied: [`crate::oauth`] follows the `resource_metadata` parameter out
//! of a `WWW-Authenticate` challenge, then follows `authorization_servers` out of
//! the document that answered it. `reqwest::Response::json` buffers whatever
//! arrives, so an unbounded read there is not a failed MCP call — it is an
//! out-of-memory kill of the whole agent process.
//!
//! The reads live here rather than at each call site because there were five of
//! them and only one had a ceiling. One helper means the next body added cannot
//! start out unbounded by omission.

use futures::StreamExt as _;
use reqwest::Response;
use serde::de::DeserializeOwned;

/// Largest OAuth metadata, token, or registration document this client buffers.
///
/// These are configuration documents, not payloads. RFC 8414 authorization-server
/// metadata and RFC 7591 registration responses from real providers run to a few
/// kilobytes; the largest field any of them carries is an access token, and a JWT
/// with an unusually rich claim set is still tens of kilobytes. One mebibyte is
/// therefore more than an order of magnitude above the largest lawful document,
/// which is what keeps this from rejecting a legitimate server — the failure this
/// bound must not cause, because a rejected metadata read means the user cannot
/// log in at all.
///
/// It is deliberately *not* [`crate::stdio::MAX_FRAME_BYTES`]. That bound is sized
/// for a JSON-RPC message carrying a 10 MiB resource blob; nothing in the OAuth
/// flow can legitimately be that large, and reusing a bound sized for a different
/// payload would leave 64 MiB of headroom for a peer with no use for it.
///
/// The same value as `zuno_llm::http::MAX_ERROR_BODY_BYTES`, for the same reason.
pub const MAX_OAUTH_BODY_BYTES: usize = 1024 * 1024;

/// Why a bounded body read did not produce a whole document.
///
/// Three variants rather than one string because the call sites treat them
/// differently: a candidate URL that answered with garbage is worth moving past,
/// while a peer that answered with more bytes than the client agreed to hold is
/// not a candidate at all.
#[derive(Debug)]
pub(crate) enum BodyError {
    /// The peer sent more than the caller's limit. Refused, never truncated.
    ///
    /// Truncating and parsing the prefix would report the breach as broken JSON,
    /// which names the wrong cause; truncating and *succeeding* would be worse,
    /// because a document is not what its first bytes say it is.
    TooLarge { reached: usize, limit: usize },
    /// The body stream failed before it ended: a connection reset, a proxy drop.
    Transport(reqwest::Error),
    /// The body arrived whole and was not the document that was expected.
    Malformed(serde_json::Error),
}

impl BodyError {
    /// Whether the peer exceeded the byte bound, as opposed to a fault that says
    /// nothing about the peer's intent.
    pub(crate) const fn is_too_large(&self) -> bool {
        matches!(self, Self::TooLarge { .. })
    }

    /// A message naming `what` was being read, for an error whose type carries no
    /// context of its own.
    pub(crate) fn describe(&self, what: &str) -> String {
        match self {
            Self::TooLarge { reached, limit } => {
                format!("{what} body reached {reached} bytes, past the {limit}-byte bound")
            }
            Self::Transport(source) => format!("{what} body could not be read: {source}"),
            Self::Malformed(source) => format!("{what} body was not the expected JSON: {source}"),
        }
    }
}

/// Reads a whole response body, refusing past `limit` instead of truncating.
///
/// The length is checked against the bytes that actually arrived, never against a
/// `Content-Length` the peer chose: a header can only ever be used to refuse
/// earlier, never to admit more, and this check has no need of it.
///
/// # What the bound bounds, exactly
///
/// `limit` bounds what this buffer ever *stores*: the refusal happens before the chunk
/// is appended, so `body` never grows past `limit`. It is not a bound on the process's
/// peak while reading. Two constant factors sit on top of it, and neither is
/// peer-chosen beyond one chunk:
///
/// - the chunk the transport has already handed over, which is why the `reached` figure
///   in [`BodyError::TooLarge`] can name a number larger than `limit` — that many bytes
///   arrived, not that many were kept;
/// - `Vec` growth, which doubles, so the allocation backing a body of length `n` can
///   have capacity up to roughly `2n`.
///
/// Pre-sizing the buffer or growing it with `reserve_exact` would trade that bounded
/// factor for a copy on every chunk, which is quadratic for a peer that answers in many
/// small chunks — a worse thing to hand an untrusted server than a factor of two. The
/// figure that matters for an OOM argument is therefore `limit` times a small constant,
/// per in-flight response per configured server, and on the streamable-HTTP path
/// `limit` is [`crate::MAX_FRAME_BYTES`].
pub(crate) async fn read_bounded_body(
    response: Response,
    limit: usize,
) -> Result<Vec<u8>, BodyError> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(BodyError::Transport)?;
        let reached = body.len().saturating_add(chunk.len());
        if reached > limit {
            return Err(BodyError::TooLarge { reached, limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Reads a bounded body and parses it into `T`.
pub(crate) async fn read_bounded_json<T>(response: Response, limit: usize) -> Result<T, BodyError>
where
    T: DeserializeOwned,
{
    let body = read_bounded_body(response, limit).await?;
    serde_json::from_slice(&body).map_err(BodyError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_what_it_refused_and_the_bound_it_enforced() {
        let error = BodyError::TooLarge {
            reached: MAX_OAUTH_BODY_BYTES + 1,
            limit: MAX_OAUTH_BODY_BYTES,
        };
        assert!(error.is_too_large());
        assert_eq!(
            error.describe("token response"),
            format!(
                "token response body reached {} bytes, past the {MAX_OAUTH_BODY_BYTES}-byte bound",
                MAX_OAUTH_BODY_BYTES + 1
            )
        );
    }

    #[test]
    fn a_malformed_body_is_not_reported_as_a_bound_breach() {
        let error = BodyError::Malformed(
            serde_json::from_slice::<serde_json::Value>(b"{").expect_err("truncated JSON"),
        );
        assert!(!error.is_too_large());
        assert!(
            error
                .describe("token response")
                .contains("not the expected")
        );
    }
}
