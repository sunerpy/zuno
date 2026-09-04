use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap};
use reqwest::{Response, Url};
use serde_json::Value;
use zuno_auth::Secret;

use crate::protocol::{ReaderFailure, fail_pending, route_message};

use super::sse::SseDecoder;
use super::transport::status_error;
use super::{RemoteError, RemoteInner, RemoteTransport};

/// Largest payload this client holds back while it waits for the `endpoint` event.
///
/// A legacy SSE server announces where to POST in an `endpoint` event, and anything it
/// sends before that has to be kept: those events are real JSON-RPC messages and
/// dropping them loses responses. So the handshake defers them — and a peer that never
/// sends `endpoint` turns that queue into an unbounded buffer of its own output, which
/// is the same out-of-memory kill the per-event cap and [`crate::MAX_OAUTH_BODY_BYTES`]
/// exist to prevent. The per-event cap does not cover it: every event can be well under
/// the cap while their number is not.
///
/// One mebibyte is orders of magnitude above anything legitimate. A conforming server
/// sends `endpoint` first; the events seen before it in practice are a keep-alive or a
/// single notification, measured in hundreds of bytes. Past this the stream is refused
/// rather than truncated, because a handshake that silently discarded the messages it
/// could not hold would lose responses and look like it worked.
const MAX_DEFERRED_HANDSHAKE_BYTES: usize = 1024 * 1024;

pub(super) async fn open_legacy(
    server: &str,
    base_url: &Url,
    http: &reqwest::Client,
    headers: &HeaderMap,
    bearer: Option<&Secret>,
    timeout: Duration,
) -> Result<(Url, Response, SseDecoder), RemoteError> {
    let mut request = http
        .get(base_url.clone())
        .headers(headers.clone())
        .header(ACCEPT, "text/event-stream");
    if let Some(bearer) = bearer {
        request = request.header(AUTHORIZATION, format!("Bearer {}", bearer.expose()));
    }
    let mut response = request.send().await.map_err(|source| {
        if source.is_timeout() {
            RemoteError::Timeout {
                server: server.to_owned(),
                elapsed: timeout,
            }
        } else {
            RemoteError::Http {
                server: server.to_owned(),
                transport: RemoteTransport::Sse,
                source,
            }
        }
    })?;
    if !response.status().is_success() {
        return Err(status_error(
            server,
            RemoteTransport::Sse,
            response.status(),
            response.headers(),
        ));
    }
    let mut decoder = SseDecoder::default();
    let mut deferred = VecDeque::new();
    let mut deferred_bytes = 0usize;
    loop {
        while let Some(event) = decoder.pop() {
            if event.event.as_deref() == Some("endpoint") {
                let endpoint =
                    base_url
                        .join(event.data.trim())
                        .map_err(|error| RemoteError::Protocol {
                            server: server.to_owned(),
                            transport: RemoteTransport::Sse,
                            message: format!("invalid legacy SSE endpoint: {error}"),
                        })?;
                decoder.prepend(deferred);
                return Ok((endpoint, response, decoder));
            }
            deferred_bytes = deferred_bytes
                .saturating_add(event.data.len())
                .saturating_add(event.event.as_deref().map_or(0, str::len));
            if deferred_bytes > MAX_DEFERRED_HANDSHAKE_BYTES {
                return Err(RemoteError::Protocol {
                    server: server.to_owned(),
                    transport: RemoteTransport::Sse,
                    message: format!(
                        "legacy SSE sent {deferred_bytes} bytes of events before its endpoint \
                         event, past the {MAX_DEFERRED_HANDSHAKE_BYTES}-byte bound"
                    ),
                });
            }
            deferred.push_back(event);
        }
        match response.chunk().await {
            Ok(Some(bytes)) => decoder
                .push(&bytes)
                .map_err(|message| RemoteError::Protocol {
                    server: server.to_owned(),
                    transport: RemoteTransport::Sse,
                    message,
                })?,
            Ok(None) => {
                return Err(RemoteError::Protocol {
                    server: server.to_owned(),
                    transport: RemoteTransport::Sse,
                    message: "legacy SSE ended before the endpoint event".to_owned(),
                });
            }
            Err(source) => {
                return Err(RemoteError::Http {
                    server: server.to_owned(),
                    transport: RemoteTransport::Sse,
                    source,
                });
            }
        }
    }
}

/// Routes legacy SSE `message` events until the stream stops being JSON-RPC.
///
/// Every exit records itself in `inner.reader_state` before failing the call that was
/// in flight, because this reader is the only thing that can deliver a response on a
/// legacy connection: [`crate::remote::RemoteClient::request`] consults that record and
/// refuses before it writes, instead of POSTing to an endpoint that still accepts
/// messages and then waiting out the deadline for an answer nothing will read.
pub(super) async fn legacy_read_loop(
    inner: Arc<RemoteInner>,
    mut response: Response,
    mut decoder: SseDecoder,
) {
    let state = Arc::clone(&inner.reader_state);
    loop {
        while let Some(event) = decoder.pop() {
            if event.event.as_deref().is_none_or(|kind| kind == "message") {
                match serde_json::from_str::<Value>(&event.data) {
                    Ok(message) => {
                        state.note_decoded();
                        route_message(
                            &inner.server,
                            &inner.pending,
                            &inner.notifications,
                            &inner.refresh,
                            message,
                        );
                    }
                    Err(error) => {
                        // Scoped to the event, not the connection, for the reason the
                        // stdio reader skips an undecodable line: the SSE decoder has
                        // already framed the events around this one, and a payload with
                        // no id cannot belong to whichever call is in flight. See
                        // [`crate::MAX_CONSECUTIVE_UNDECODABLE_FRAMES`].
                        let run = state.note_undecodable(&event.data);
                        if run.loud {
                            tracing::warn!(
                                server = %inner.server,
                                %error,
                                undecodable = run.count,
                                "legacy MCP SSE event was not JSON"
                            );
                        } else {
                            tracing::debug!(
                                server = %inner.server,
                                undecodable = run.count,
                                "legacy MCP SSE event was not JSON"
                            );
                        }
                        if let Some(failure) = run.violation {
                            state.note_exit(failure.clone());
                            let in_flight = fail_pending(&inner.pending, failure);
                            tracing::warn!(
                                server = %inner.server,
                                undecodable = run.count,
                                in_flight,
                                "legacy MCP SSE carried no decodable event within the \
                                 undecodable-frame bound; the stream was ended"
                            );
                            return;
                        }
                    }
                }
            }
        }
        match response.chunk().await {
            Ok(Some(bytes)) => {
                if let Err(error) = decoder.push(&bytes) {
                    tracing::warn!(server = %inner.server, %error, "legacy MCP SSE was not UTF-8");
                    // Not a framing violation of one event but a byte stream this
                    // client cannot decode at all, which is how the stdio reader
                    // reports the same fault.
                    let failure = ReaderFailure::Io {
                        kind: std::io::ErrorKind::InvalidData,
                        message: Arc::from(error),
                    };
                    state.note_exit(failure.clone());
                    fail_pending(&inner.pending, failure);
                    return;
                }
            }
            Ok(None) => {
                // A stream that ended having sent events but never a decodable one is
                // a peer that was not speaking JSON-RPC here, and saying so names the
                // fault a bare close would hide.
                let failure = state.not_json_rpc().unwrap_or(ReaderFailure::Closed);
                state.note_exit(failure.clone());
                fail_pending(&inner.pending, failure);
                return;
            }
            Err(error) => {
                let failure = ReaderFailure::Io {
                    kind: std::io::ErrorKind::Other,
                    message: Arc::from(error.to_string()),
                };
                state.note_exit(failure.clone());
                fail_pending(&inner.pending, failure);
                return;
            }
        }
    }
}
