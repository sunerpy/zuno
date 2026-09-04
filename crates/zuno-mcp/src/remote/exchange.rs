use std::sync::Arc;
use std::sync::atomic::Ordering;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Response, Url};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::body::read_bounded_body;
use crate::protocol::{
    ExchangeError, ReaderFailure, decode_response, lock, may_have_side_effects,
    reader_failure_label, route_message,
};
use crate::stdio::MAX_FRAME_BYTES;

use super::legacy::legacy_read_loop;
use super::sse::SseDecoder;
use super::transport::status_error;
use super::{PROTOCOL_HEADER, RemoteClient, RemoteError, RemoteTransport, SESSION_HEADER};

impl RemoteClient {
    pub(super) async fn request(&self, method: &str, params: Value) -> Result<Value, RemoteError> {
        let _operation = self.inner.operation.lock().await;
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(self.protocol_error("connection is closed"));
        }
        // The reader is the only thing that can deliver a response, so once it has
        // stopped this request can never be answered. Refusing *before* the write
        // is what keeps the refusal honest: nothing reached the server, so a
        // definite failure is not a claim about a side effect. Without this check
        // the write succeeds, the call waits out the whole deadline, and the
        // failure comes back as a retryable timeout against a permanently deaf
        // connection.
        if let Some(failure) = self.inner.reader_state.exit() {
            return Err(self.protocol_error(format!(
                "connection is unusable: {}",
                ExchangeError::from(failure)
            )));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let (sender, receiver) = oneshot::channel();
        lock(&self.inner.pending).insert(id, sender);
        let exchange = async {
            match self.inner.transport {
                RemoteTransport::StreamableHttp => self.send_streamable(method, &message).await?,
                RemoteTransport::Sse => {
                    self.ensure_legacy_reader().await?;
                    self.send_legacy(&message).await?;
                }
            }
            let response = receiver
                .await
                .map_err(|_| self.protocol_error("response channel closed"))?;
            let response = response.map_err(|error| self.reader_failure_error(method, error))?;
            decode_response(method, response)
                .map_err(|error| self.protocol_error(error.to_string()))
        };
        let result = match tokio::time::timeout(self.inner.timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(self.deadline_error()),
        };
        lock(&self.inner.pending).remove(&id);
        result
    }

    pub(super) async fn send_initialized(&self) -> Result<(), RemoteError> {
        let message = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        match self.inner.transport {
            RemoteTransport::StreamableHttp => {
                let response = self
                    .request_builder(reqwest::Method::POST, self.inner.base_url.clone())
                    .json(&message)
                    .send()
                    .await
                    .map_err(|source| self.http_error(source))?;
                self.capture_session(&response).await;
                if response.status().is_success() {
                    Ok(())
                } else {
                    Err(self.status_error(&response))
                }
            }
            RemoteTransport::Sse => self.send_legacy(&message).await,
        }
    }

    async fn send_streamable(&self, method: &str, message: &Value) -> Result<(), RemoteError> {
        let response = self
            .request_builder(reqwest::Method::POST, self.inner.base_url.clone())
            .json(message)
            .send()
            .await
            .map_err(|source| self.http_error(source))?;
        self.capture_session(&response).await;
        if !response.status().is_success() {
            return Err(self.status_error(&response));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            self.consume_streamable_sse(method, response).await
        } else {
            let message = self.read_bounded_message(method, response).await?;
            self.route(message);
            Ok(())
        }
    }

    /// Reads a non-SSE streamable-HTTP body under the shared message bound.
    ///
    /// `Response::json` buffers whatever the peer sends. This branch is chosen by
    /// nothing but a `Content-Type` header, so an unbounded read here is one header
    /// away from bypassing the SSE decoder's event cap — and an OOM kill takes the
    /// whole agent down, not just the MCP call. [`MAX_FRAME_BYTES`] is the bound
    /// rather than the SSE event cap because this body is one whole JSON-RPC message,
    /// which is what that bound is already sized to admit, and rather than
    /// [`crate::MAX_OAUTH_BODY_BYTES`] because a lawful `resources/read` reply
    /// carries a 10 MiB blob as roughly 13.4 MiB of base64.
    ///
    /// # A deliberate recovery reclassification, and who it may not be applied to
    ///
    /// A body that arrives whole and is not JSON reports a *permanent* failure where
    /// `Response::json` reported a `reqwest` decode error (transient). That is right
    /// for a protocol violation, and it also captures the narrower case of a body that
    /// ended early *without* a transport error — an HTTP/1.1 `Connection: close` reply
    /// with no content-length, cut by a proxy — which used to be retried. The trade is
    /// taken knowingly: this client cannot tell that case apart from a server that
    /// emits invalid JSON, and treating every unparseable body as retryable is what
    /// makes a genuinely broken server absorb the whole retry budget.
    ///
    /// What it may *not* do is report that as a definite failure of a call that may
    /// already have run. This body arrives after the request reached the server, so an
    /// unreadable answer to `tools/call` leaves the outcome unknown, exactly as a
    /// reader that stops does ([`Self::reader_failure_error`]). Both failures here go
    /// through [`Self::unreadable_reply_error`], which keeps the permanent framing
    /// error for read-only methods and reports the uncertain class for the rest.
    async fn read_bounded_message(
        &self,
        method: &str,
        response: Response,
    ) -> Result<Value, RemoteError> {
        let body = read_bounded_body(response, MAX_FRAME_BYTES)
            .await
            // Refused rather than truncated: a truncated body parses as broken
            // JSON, which names the wrong cause.
            .map_err(|error| self.unreadable_reply_error(method, error.describe("response")))?;
        serde_json::from_slice(&body).map_err(|error| {
            self.unreadable_reply_error(method, format!("response body was not JSON: {error}"))
        })
    }

    /// Reports a reply this client could not read, classed by what the call may have done.
    ///
    /// Every caller of this has already had its request written and accepted by the
    /// server: the failure is in the *answer*, not in the delivery. So for a method
    /// that can have run a side effect the honest report is that the outcome is
    /// unknown — [`RemoteError::Timeout`] is the only class this crate has that says so
    /// ([`super::RemoteFailureKind::Timeout`] — "the request may already have taken
    /// effect") — and the tool proxy declares
    /// [`zuno_tool::ToolReplayPolicy::Never`], so it reaches the model as "inspect
    /// authoritative state", not as "call it again".
    ///
    /// [`RemoteError::Protocol`] (permanent) is kept for read-only methods, where
    /// nothing was mutated and naming the framing fault is the whole diagnosis. The
    /// bound breach is included rather than kept permanent for every method: a peer
    /// that answered with more bytes than this client agreed to hold *did* answer, so
    /// the call almost certainly ran, and refusing to say so is the direction that
    /// invites a duplicate side effect. The cost of the uncertain class here is bounded
    /// — the read is capped at [`MAX_FRAME_BYTES`] per attempt and requests on one
    /// client are serialized by `operation`, so a model that calls again cannot raise
    /// the peak, only repeat it.
    fn unreadable_reply_error(&self, method: &str, detail: impl Into<String>) -> RemoteError {
        if !may_have_side_effects(method) {
            return self.protocol_error(detail);
        }
        let detail = detail.into();
        tracing::warn!(
            server = %self.inner.server,
            method,
            // Not `message`: `zuno_observability` leaves that one field name readable
            // and prints it with no `name=` prefix, so a value carrying peer-shaped
            // detail would land in the plaintext log and the `logs.sqlite` message
            // column as if it were Zuno's own sentence.
            stream_output = %detail,
            "remote MCP answered a call that may have taken effect with a reply this \
             client could not read; its outcome is unknown"
        );
        RemoteError::Timeout {
            server: self.inner.server.clone(),
            elapsed: self.inner.timeout,
        }
    }

    /// Turns a reader failure into the error class the peer actually justified.
    ///
    /// A reader that stops has already had this request written to the wire, so for
    /// a method that can have run a side effect the honest report is that the
    /// outcome is *unknown*, not that the call definitely failed:
    /// [`RemoteError::Timeout`] is the only class this crate has that says so
    /// ([`super::RemoteFailureKind::Timeout`] — "the request may already have taken
    /// effect"). That holds for every way a reader can stop, and most obviously for
    /// undecodable frames, which carry no JSON-RPC id and therefore prove nothing at
    /// all about whichever call was outstanding. Read-only methods keep the permanent
    /// framing error, which is what names the fault.
    fn reader_failure_error(&self, method: &str, failure: ReaderFailure) -> RemoteError {
        let failure_label = reader_failure_label(&failure);
        let detail = ExchangeError::from(failure).to_string();
        if may_have_side_effects(method) {
            tracing::warn!(
                server = %self.inner.server,
                method,
                // The class is safe to render; the sentence is not.
                // `ExchangeError::NotJsonRpc` embeds a bounded excerpt of the peer's own
                // stream, and `zuno_observability` prints a field named `message`
                // verbatim, without a `name=` prefix, into the plaintext log and the
                // `logs.sqlite` message column. `stream_output` ends in a payload word,
                // so policy scrubs it.
                failure = failure_label,
                stream_output = %detail,
                "remote MCP stream ended while a side-effecting call was outstanding; \
                 its outcome is unknown"
            );
            return RemoteError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            };
        }
        self.protocol_error(detail)
    }

    /// The deadline this call spent, named by what the stream had produced by then.
    ///
    /// A peer that answered nothing stays a retryable timeout. A legacy SSE stream
    /// that delivered events and never once framed a JSON-RPC message is the same
    /// misconfiguration the stdio reader reports the same way: no number of retries
    /// turns that peer into an MCP server, so the deadline names the framing violation
    /// and the events that prove it.
    fn deadline_error(&self) -> RemoteError {
        match self.inner.reader_state.not_json_rpc() {
            Some(failure) => self.protocol_error(ExchangeError::from(failure).to_string()),
            None => RemoteError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            },
        }
    }

    /// Reads the per-request SSE stream until it carries this call's reply.
    ///
    /// Every failure here happens *after* the POST was accepted, so each one goes
    /// through [`Self::unreadable_reply_error`] for the same reason the non-SSE body
    /// does: an event this client cannot parse, a stream that ends before a reply, and
    /// a decoder that refuses the stream all leave a side-effecting call's outcome
    /// unknown rather than definitely failed.
    ///
    /// The one exception is a transport failure mid-body. For a read-only method that
    /// keeps [`Self::http_error`]'s transient class, because a network fault genuinely
    /// may succeed on another attempt and calling it permanent would break a working
    /// server on one dropped connection; for a side-effecting method transient is the
    /// dangerous direction — it invites a mechanical replay of a call that may have
    /// landed — so it reports the uncertain class instead.
    async fn consume_streamable_sse(
        &self,
        method: &str,
        mut response: Response,
    ) -> Result<(), RemoteError> {
        let mut decoder = SseDecoder::default();
        loop {
            while let Some(event) = decoder.pop() {
                if event.event.as_deref().is_none_or(|kind| kind == "message") {
                    let message = serde_json::from_str::<Value>(&event.data).map_err(|error| {
                        self.unreadable_reply_error(method, format!("invalid SSE JSON: {error}"))
                    })?;
                    let is_response = message.get("id").is_some();
                    self.route(message);
                    if is_response {
                        return Ok(());
                    }
                }
            }
            match response.chunk().await {
                Ok(Some(bytes)) => decoder
                    .push(&bytes)
                    .map_err(|error| self.unreadable_reply_error(method, error))?,
                Ok(None) => {
                    return Err(
                        self.unreadable_reply_error(method, "SSE response ended before a reply")
                    );
                }
                Err(source) => {
                    return Err(if may_have_side_effects(method) {
                        self.unreadable_reply_error(
                            method,
                            format!("SSE response body failed: {source}"),
                        )
                    } else {
                        self.http_error(source)
                    });
                }
            }
        }
    }

    async fn send_legacy(&self, message: &Value) -> Result<(), RemoteError> {
        let endpoint = self
            .inner
            .legacy
            .as_ref()
            .expect("legacy transport has an endpoint")
            .endpoint
            .clone();
        let response = self
            .request_builder(reqwest::Method::POST, endpoint)
            .json(message)
            .send()
            .await
            .map_err(|source| self.http_error(source))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(self.status_error(&response))
        }
    }

    /// Starts the legacy reader once, and never restarts one that has stopped.
    ///
    /// A stopped reader is reported by [`Self::request`] before the write, so this
    /// only has to avoid spawning a second reader over a source that is already
    /// taken.
    async fn ensure_legacy_reader(&self) -> Result<(), RemoteError> {
        let legacy = self
            .inner
            .legacy
            .as_ref()
            .expect("legacy transport has state");
        if lock(&legacy.reader).is_some() {
            return Ok(());
        }
        let Some((response, decoder)) = legacy.source.lock().await.take() else {
            return Err(self.protocol_error("legacy SSE source is unavailable"));
        };
        let inner = Arc::clone(&self.inner);
        let reader = tokio::spawn(async move { legacy_read_loop(inner, response, decoder).await });
        *lock(&legacy.reader) = Some(reader);
        Ok(())
    }

    pub(super) fn request_builder(
        &self,
        method: reqwest::Method,
        url: Url,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .inner
            .http
            .request(method, url)
            .headers(self.inner.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json");
        if let Some(bearer) = &self.inner.bearer {
            request = request.header(AUTHORIZATION, format!("Bearer {}", bearer.expose()));
        }
        if let Ok(session) = self.inner.session_id.try_lock()
            && let Some(session) = session.as_ref()
        {
            request = request.header(SESSION_HEADER, session.clone());
        }
        if let Some(initialization) = self.inner.initialization.get() {
            request = request.header(PROTOCOL_HEADER, &initialization.protocol_version);
        }
        request
    }

    async fn capture_session(&self, response: &Response) {
        if let Some(value) = response.headers().get(SESSION_HEADER) {
            *self.inner.session_id.lock().await = Some(value.clone());
        }
    }

    fn route(&self, message: Value) {
        route_message(
            &self.inner.server,
            &self.inner.pending,
            &self.inner.notifications,
            &self.inner.refresh,
            message,
        );
    }

    pub(super) fn protocol_error(&self, message: impl Into<String>) -> RemoteError {
        RemoteError::Protocol {
            server: self.inner.server.clone(),
            transport: self.inner.transport,
            message: message.into(),
        }
    }

    fn http_error(&self, source: reqwest::Error) -> RemoteError {
        if source.is_timeout() {
            RemoteError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            }
        } else {
            RemoteError::Http {
                server: self.inner.server.clone(),
                transport: self.inner.transport,
                source,
            }
        }
    }

    fn status_error(&self, response: &Response) -> RemoteError {
        status_error(
            &self.inner.server,
            self.inner.transport,
            response.status(),
            response.headers(),
        )
    }
}
