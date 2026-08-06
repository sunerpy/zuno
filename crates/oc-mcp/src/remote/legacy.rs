use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use oc_auth::Secret;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap};
use reqwest::{Response, Url};
use serde_json::Value;

use crate::protocol::{ReaderFailure, fail_pending, route_message};

use super::sse::SseDecoder;
use super::transport::status_error;
use super::{RemoteError, RemoteInner, RemoteTransport};

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

pub(super) async fn legacy_read_loop(
    inner: Arc<RemoteInner>,
    mut response: Response,
    mut decoder: SseDecoder,
) {
    loop {
        while let Some(event) = decoder.pop() {
            if event.event.as_deref().is_none_or(|kind| kind == "message") {
                match serde_json::from_str::<Value>(&event.data) {
                    Ok(message) => route_message(
                        &inner.server,
                        &inner.pending,
                        &inner.notifications,
                        &inner.refresh,
                        message,
                    ),
                    Err(error) => {
                        tracing::warn!(server = %inner.server, %error, "legacy MCP SSE event was not JSON");
                        fail_pending(
                            &inner.pending,
                            ReaderFailure::Decode {
                                line: Arc::from(event.data),
                            },
                        );
                    }
                }
            }
        }
        match response.chunk().await {
            Ok(Some(bytes)) => {
                if let Err(error) = decoder.push(&bytes) {
                    tracing::warn!(server = %inner.server, %error, "legacy MCP SSE was not UTF-8");
                    fail_pending(
                        &inner.pending,
                        ReaderFailure::Decode {
                            line: Arc::from(error),
                        },
                    );
                    return;
                }
            }
            Ok(None) => {
                fail_pending(&inner.pending, ReaderFailure::Closed);
                return;
            }
            Err(error) => {
                fail_pending(
                    &inner.pending,
                    ReaderFailure::Io {
                        kind: std::io::ErrorKind::Other,
                        message: Arc::from(error.to_string()),
                    },
                );
                return;
            }
        }
    }
}
