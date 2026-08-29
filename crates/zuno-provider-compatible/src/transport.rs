//! The seam between this profile and the network.
//!
//! # Why a trait
//!
//! Every test in this crate replays recorded bytes. That is not a convention that
//! a later task could quietly break: [`Provider::stream`] reaches the wire only
//! through [`Transport`], and the tests construct the provider with a transport
//! that reads a cassette. There is no `#[cfg(test)]` branch inside the request
//! path, so the "no live provider call in a test" rule holds structurally rather
//! than by inspection.
//!
//! It also keeps `reqwest` out of the translation logic, which is the part with
//! actual behaviour worth testing.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::{Stream, StreamExt};
use serde_json::Value;
use zuno_error::ProviderError;
use zuno_llm::sse::{MAX_PROVIDER_WAIT, STREAM_IDLE_TIMEOUT_ENV, StreamIdleTimeout};

use crate::stream::retry_after;
use crate::wire::ErrorEnvelope;

/// A stream of raw response chunks, exactly as the transport received them.
///
/// Bytes, not text. Decoding is [`zuno_llm::sse::SseParser`]'s job, because only it
/// holds the boundary state that makes a code point split across two chunks
/// survive.
pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, ProviderError>> + Send>>;

/// Per-request HTTP timeouts resolved from provider configuration.
///
/// A whole-request timeout spans response headers and every streamed body chunk.
/// Header and chunk timeouts are phase-specific; the earliest applicable
/// deadline wins without changing the other phase's policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HttpTimeouts {
    total: Option<Duration>,
    header: Option<Duration>,
    chunk: Option<Duration>,
}

impl HttpTimeouts {
    #[must_use]
    pub const fn new(
        total: Option<Duration>,
        header: Option<Duration>,
        chunk: Option<Duration>,
    ) -> Self {
        Self {
            total,
            header,
            chunk,
        }
    }

    #[must_use]
    pub const fn total(self) -> Option<Duration> {
        self.total
    }

    #[must_use]
    pub const fn header(self) -> Option<Duration> {
        self.header
    }

    #[must_use]
    pub const fn chunk(self) -> Option<Duration> {
        self.chunk
    }
}

/// The default maximum silence between response-body chunks.
///
/// This leaves thirty seconds beyond the ninety-second reasoning gaps already
/// accepted by the shared SSE policy, while still terminating before the
/// two-hundred-second liveness probe that exposed the unbounded read.
const DEFAULT_RESPONSE_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// The largest override that still satisfies the two-hundred-second liveness probe.
///
/// The twenty-second margin leaves time for scheduling and error propagation after
/// the idle read itself expires.
const MAX_RESPONSE_IDLE_TIMEOUT: Duration = MAX_PROVIDER_WAIT;

fn bounded_response_idle_timeout(idle: StreamIdleTimeout) -> StreamIdleTimeout {
    StreamIdleTimeout::new(idle.duration().min(MAX_RESPONSE_IDLE_TIMEOUT))
}

/// One outbound request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Fully-resolved absolute URL.
    pub url: String,
    /// Headers to send, ordered so a test can compare them.
    pub headers: BTreeMap<String, String>,
    /// The JSON body.
    pub body: Value,
    /// Whole-request, response-header, and streamed-chunk timeouts.
    pub timeouts: HttpTimeouts,
}

/// How a request reaches a server.
pub trait Transport: fmt::Debug + Send + Sync + 'static {
    /// Send `request` and return its response chunks.
    ///
    /// # Errors
    ///
    /// A typed [`ProviderError`]. A non-2xx status is an error here rather than a
    /// stream item, so a caller cannot accidentally translate an error body as if
    /// it were a chunk.
    fn send(
        &self,
        request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChunkStream, ProviderError>> + Send + '_>>;
}

/// The production transport.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
    provider: String,
    idle: StreamIdleTimeout,
}

impl ReqwestTransport {
    /// A transport for `provider`, using a fresh client.
    #[must_use]
    pub fn new(provider: impl Into<String>) -> Self {
        Self::with_client(provider, zuno_network::client())
    }

    /// A transport for `provider` sharing an existing client.
    ///
    /// Sharing matters: a client owns the connection pool, and one per provider
    /// per process is the difference between reusing a TLS session and
    /// renegotiating on every turn.
    #[must_use]
    pub fn with_client(provider: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            client,
            provider: provider.into(),
            idle: bounded_response_idle_timeout(StreamIdleTimeout::from_config(
                DEFAULT_RESPONSE_IDLE_TIMEOUT,
            )),
        }
    }

    /// Override the maximum silence allowed between response chunks.
    ///
    /// This is an idle bound, not a total request deadline. Every received chunk
    /// starts a fresh allowance. Overrides are capped by the production liveness
    /// policy.
    #[must_use]
    pub fn with_idle_timeout(mut self, idle: StreamIdleTimeout) -> Self {
        self.idle = bounded_response_idle_timeout(idle);
        self
    }
}

impl Transport for ReqwestTransport {
    fn send(
        &self,
        request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChunkStream, ProviderError>> + Send + '_>> {
        let client = self.client.clone();
        let provider = self.provider.clone();
        let idle = self.idle;
        Box::pin(async move {
            let started = tokio::time::Instant::now();
            let total_deadline = request
                .timeouts
                .total()
                .map(|duration| Deadline::after(started, duration, TimeoutPhase::WholeRequest));
            let header_deadline = request
                .timeouts
                .header()
                .map(|duration| Deadline::after(started, duration, TimeoutPhase::ResponseHeaders));
            let mut builder = client.post(&request.url).json(&request.body);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            let response = wait_until(
                &provider,
                earliest(total_deadline, header_deadline),
                builder.send(),
            )
            .await?
            .map_err(ProviderError::transient)?;

            let status = response.status();
            if !status.is_success() {
                let header = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(retry_after);
                // Read as bytes and decode strictly: a non-UTF-8 error body loses
                // its detail rather than being rendered with replacement
                // characters that could be mistaken for the vendor's own text.
                let bytes = wait_until(&provider, total_deadline, response.bytes())
                    .await?
                    .map_err(ProviderError::transient)?;
                let text = std::str::from_utf8(&bytes).ok().map(str::to_owned);
                return Err(classify_response(
                    &provider,
                    status.as_u16(),
                    header,
                    text.as_deref(),
                ));
            }

            let body = Box::pin(response.bytes_stream());
            let chunk_timeout = request.timeouts.chunk().unwrap_or_else(|| idle.duration());
            let chunks = futures::stream::unfold(
                Some((body, provider, total_deadline, chunk_timeout)),
                |state| async move {
                    let (mut body, provider, total_deadline, chunk_timeout) = state?;
                    let chunk_deadline = Deadline::after(
                        tokio::time::Instant::now(),
                        chunk_timeout,
                        TimeoutPhase::ResponseChunk,
                    );
                    match wait_until(
                        &provider,
                        earliest(total_deadline, Some(chunk_deadline)),
                        body.next(),
                    )
                    .await
                    {
                        Ok(Some(Ok(bytes))) => Some((
                            Ok(bytes.to_vec()),
                            Some((body, provider, total_deadline, chunk_timeout)),
                        )),
                        Ok(Some(Err(error))) => Some((Err(ProviderError::transient(error)), None)),
                        Ok(None) => None,
                        Err(error) => Some((Err(error), None)),
                    }
                },
            );
            Ok(Box::pin(chunks) as ChunkStream)
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Deadline {
    at: tokio::time::Instant,
    duration: Duration,
    phase: TimeoutPhase,
}

impl Deadline {
    fn after(started: tokio::time::Instant, duration: Duration, phase: TimeoutPhase) -> Self {
        Self {
            at: started + duration,
            duration,
            phase,
        }
    }
}

fn earliest(left: Option<Deadline>, right: Option<Deadline>) -> Option<Deadline> {
    match (left, right) {
        (Some(left), Some(right)) if left.at <= right.at => Some(left),
        (Some(_), Some(right)) => Some(right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

async fn wait_until<F, T>(
    provider: &str,
    deadline: Option<Deadline>,
    future: F,
) -> Result<T, ProviderError>
where
    F: Future<Output = T>,
{
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.at, future)
            .await
            .map_err(|_| {
                ProviderError::transient(HttpTimeoutError {
                    provider: provider.to_owned(),
                    duration: deadline.duration,
                    phase: deadline.phase,
                })
            }),
        None => Ok(future.await),
    }
}

#[derive(Debug, Clone, Copy)]
enum TimeoutPhase {
    WholeRequest,
    ResponseHeaders,
    ResponseChunk,
}

impl TimeoutPhase {
    const fn description(self) -> &'static str {
        match self {
            Self::WholeRequest => "whole request timeout",
            Self::ResponseHeaders => "response headers timeout",
            Self::ResponseChunk => "response stream idle timeout",
        }
    }
}

#[derive(Debug)]
struct HttpTimeoutError {
    provider: String,
    duration: Duration,
    phase: TimeoutPhase,
}

impl fmt::Display for HttpTimeoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provider `{}` {} after {:?}",
            self.provider,
            self.phase.description(),
            self.duration
        )?;
        if matches!(self.phase, TimeoutPhase::ResponseChunk) {
            write!(
                formatter,
                "; raise provider options.chunkTimeout or {STREAM_IDLE_TIMEOUT_ENV} for slower providers"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpTimeoutError {}

/// Classify a non-2xx response into the typed taxonomy.
///
/// The HTTP status is authoritative for the recovery class. The body refines it
/// only where the status genuinely cannot say which of two classes applies: a
/// `400` carrying `context_length_exceeded` needs compaction rather than failure,
/// and a `400` carrying `content_filter` is a refusal. Both of those are
/// *structured code* reads. The vendor's prose is attached as a source for the
/// human and is never examined.
#[must_use]
pub fn classify_response(
    provider: &str,
    status: u16,
    retry_after_header: Option<std::time::Duration>,
    body: Option<&str>,
) -> ProviderError {
    let wire = body
        .and_then(|text| serde_json::from_str::<ErrorEnvelope>(text).ok())
        .map(ErrorEnvelope::into_error);

    if let Some(error) = &wire {
        match error.code_str() {
            Some("context_length_exceeded") => {
                return ProviderError::ContextLimit {
                    limit_tokens: None,
                    used_tokens: None,
                };
            }
            Some("content_filter") => {
                return ProviderError::Refused {
                    provider: provider.to_owned(),
                    provider_text: error.message.clone(),
                };
            }
            _ => {}
        }
        if error.kind.as_deref() == Some("content_filter") {
            return ProviderError::Refused {
                provider: provider.to_owned(),
                provider_text: error.message.clone(),
            };
        }
    }

    if status == 429 {
        return ProviderError::RateLimited {
            retry_after: retry_after_header,
        };
    }

    let detail = ResponseBody {
        provider: provider.to_owned(),
        status,
        body: body.map(truncate),
    };
    match ProviderError::from_status(provider, status) {
        ProviderError::Auth { provider, .. } => ProviderError::Auth {
            provider,
            source: Some(Box::new(detail)),
        },
        ProviderError::Transient { status, .. } => ProviderError::Transient {
            status,
            source: Some(Box::new(detail)),
        },
        ProviderError::Fatal { status, .. } => ProviderError::Fatal {
            status,
            source: Some(Box::new(detail)),
        },
        // `from_status` returns only those three plus `RateLimited`, which the
        // branch above already handled. Listing the rest keeps this exhaustive so
        // a new variant forces a decision here.
        other @ (ProviderError::RateLimited { .. }
        | ProviderError::ContextLimit { .. }
        | ProviderError::Refused { .. }
        | ProviderError::UnsupportedCapability { .. }) => other,
    }
}

/// How much of a vendor error body is worth keeping in a log line.
const BODY_LIMIT: usize = 512;

fn truncate(body: &str) -> String {
    if body.len() <= BODY_LIMIT {
        return body.to_owned();
    }
    // Cut on a character boundary; a byte slice of UTF-8 can split a code point.
    let end = body
        .char_indices()
        .take_while(|(index, _)| *index <= BODY_LIMIT)
        .last()
        .map_or(0, |(index, _)| index);
    format!("{}…", &body[..end])
}

/// The vendor's own error text, kept for display.
#[derive(Debug)]
struct ResponseBody {
    provider: String,
    status: u16,
    body: Option<String>,
}

impl fmt::Display for ResponseBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provider `{}` returned HTTP {}",
            self.provider, self.status
        )?;
        if let Some(body) = &self.body {
            write!(formatter, ": {body}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ResponseBody {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use zuno_error::Recovery;
    use zuno_llm::sse::StreamIdleTimeout;

    async fn spawn_chunked_server(
        chunks: Vec<(Duration, &'static [u8])>,
        finish: bool,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind chunked-response fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 4096];
            let bytes = socket.read(&mut request).await.expect("read request");
            assert!(
                request[..bytes].starts_with(b"POST "),
                "fixture received an unexpected HTTP request"
            );
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\
                      Connection: close\r\n\
                      \r\n",
                )
                .await
                .expect("write response headers");

            for (delay, chunk) in chunks {
                tokio::time::sleep(delay).await;
                socket
                    .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                    .await
                    .expect("write chunk size");
                socket.write_all(chunk).await.expect("write response chunk");
                socket
                    .write_all(b"\r\n")
                    .await
                    .expect("terminate response chunk");
            }

            if finish {
                socket
                    .write_all(b"0\r\n\r\n")
                    .await
                    .expect("finish chunked response");
            } else {
                std::future::pending::<()>().await;
            }
        });
        (address, server)
    }

    #[test]
    fn a_429_carries_the_delay_the_vendor_named() {
        let error = classify_response(
            "groq",
            429,
            Some(Duration::from_secs(12)),
            Some(r#"{"error":{"message":"rate limit"}}"#),
        );
        assert_eq!(
            error.recovery(),
            Recovery::Retry {
                after: Some(Duration::from_secs(12))
            }
        );
    }

    #[test]
    fn a_400_naming_context_length_asks_for_compaction() {
        let error = classify_response(
            "deepseek",
            400,
            None,
            Some(r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#),
        );
        assert_eq!(error.recovery(), Recovery::Compact);
    }

    #[test]
    fn a_401_asks_for_reauthentication_and_keeps_the_body_as_a_source() {
        use std::error::Error as _;
        let error = classify_response("openrouter", 401, None, Some(r#"{"message":"no key"}"#));
        assert_eq!(error.recovery(), Recovery::Reauthenticate);
        let source = error.source().expect("body detail").to_string();
        assert!(source.contains("openrouter"), "{source}");
        assert!(source.contains("401"), "{source}");
    }

    #[test]
    fn a_503_is_retryable_and_a_422_is_not() {
        assert_eq!(
            classify_response("mistral", 503, None, None).recovery(),
            Recovery::Retry { after: None }
        );
        assert_eq!(
            classify_response("mistral", 422, None, None).recovery(),
            Recovery::Fail
        );
    }

    #[test]
    fn a_non_json_body_still_classifies_from_the_status_alone() {
        let error = classify_response("venice", 502, None, Some("<html>bad gateway</html>"));
        assert_eq!(error.recovery(), Recovery::Retry { after: None });
    }

    #[test]
    fn a_long_body_is_truncated_on_a_character_boundary() {
        let body = "。".repeat(400);
        let cut = truncate(&body);
        assert!(cut.len() <= BODY_LIMIT + 4, "{}", cut.len());
        assert!(cut.ends_with('…'));
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn a_truncated_error_body_surfaces_the_read_failure_as_transient() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind truncated-body fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 4096];
            let bytes = socket.read(&mut request).await.expect("read request");
            assert!(
                request[..bytes].starts_with(b"POST "),
                "fixture received an unexpected HTTP request"
            );
            socket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\n\
                      Content-Type: application/json\r\n\
                      Content-Length: 128\r\n\
                      Connection: close\r\n\
                      \r\n\
                      {\"error\":{\"code\":\"context_length_exceeded\"}}",
                )
                .await
                .expect("write deliberately truncated response");
        });

        let transport = ReqwestTransport::new("truncated-fixture");
        let result = transport
            .send(HttpRequest {
                url: format!("http://{address}/chat/completions"),
                headers: BTreeMap::new(),
                body: serde_json::json!({}),
                timeouts: HttpTimeouts::default(),
            })
            .await;
        server.await.expect("fixture task");

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a truncated error body must not become a response stream"),
        };
        assert!(error.is_retryable(), "{error:?}");
        assert!(matches!(
            error,
            ProviderError::Transient {
                status: None,
                source: Some(_)
            }
        ));
        let cause = error.source().expect("body read cause").to_string();
        assert!(
            cause.contains("body") || cause.contains("connection"),
            "unexpected body read cause: {cause}"
        );
    }

    #[tokio::test]
    async fn a_stalled_stream_times_out_after_preserving_received_chunks() {
        let (address, server) =
            spawn_chunked_server(vec![(Duration::ZERO, b"PARTIAL_")], false).await;
        let transport = ReqwestTransport::new("stalled-fixture");
        let mut chunks = transport
            .send(HttpRequest {
                url: format!("http://{address}/chat/completions"),
                headers: BTreeMap::new(),
                body: serde_json::json!({}),
                timeouts: HttpTimeouts::new(None, None, Some(Duration::from_millis(75))),
            })
            .await
            .expect("response headers should arrive");

        let partial = tokio::time::timeout(Duration::from_secs(1), chunks.next())
            .await
            .expect("the first response chunk should arrive")
            .expect("the stream should contain the partial chunk")
            .expect("the partial chunk should be successful");
        assert_eq!(partial, b"PARTIAL_");

        let error = tokio::time::timeout(Duration::from_secs(1), chunks.next())
            .await
            .expect("the stalled read must be bounded")
            .expect("the idle timeout must be a stream item")
            .expect_err("a held-open socket must fail after its idle allowance");
        assert!(matches!(error, ProviderError::Transient { .. }));
        let cause = error.source().expect("idle timeout cause").to_string();
        assert!(cause.contains("idle timeout"), "{cause}");
        assert!(cause.contains("stalled-fixture"), "{cause}");

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn a_whole_request_timeout_spans_the_streamed_body() {
        let (address, server) =
            spawn_chunked_server(vec![(Duration::ZERO, b"PARTIAL_")], false).await;
        let transport = ReqwestTransport::new("whole-timeout-fixture");
        let mut chunks = transport
            .send(HttpRequest {
                url: format!("http://{address}/responses"),
                headers: BTreeMap::new(),
                body: serde_json::json!({}),
                timeouts: HttpTimeouts::new(
                    Some(Duration::from_millis(150)),
                    None,
                    Some(Duration::from_secs(1)),
                ),
            })
            .await
            .expect("response headers should arrive");

        let partial = chunks
            .next()
            .await
            .expect("the stream should contain the partial chunk")
            .expect("the partial chunk should be successful");
        assert_eq!(partial, b"PARTIAL_");

        let error = tokio::time::timeout(Duration::from_secs(1), chunks.next())
            .await
            .expect("the whole-request timeout must remain active")
            .expect("the timeout must be a stream item")
            .expect_err("a held-open response must fail at the whole-request deadline");
        assert!(matches!(error, ProviderError::Transient { .. }));
        let cause = error.source().expect("whole timeout cause").to_string();
        assert!(cause.contains("whole request"), "{cause}");
        assert!(cause.contains("150ms"), "{cause}");

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn a_configured_header_timeout_bounds_the_wait_for_response_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind header-timeout fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 4096];
            let bytes = socket.read(&mut request).await.expect("read request");
            assert!(
                request[..bytes].starts_with(b"POST "),
                "fixture received an unexpected HTTP request"
            );
            std::future::pending::<()>().await;
        });
        let transport = ReqwestTransport::new("header-timeout-fixture");

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            transport.send(HttpRequest {
                url: format!("http://{address}/responses"),
                headers: BTreeMap::new(),
                body: serde_json::json!({}),
                timeouts: HttpTimeouts::new(None, Some(Duration::from_millis(75)), None),
            }),
        )
        .await
        .expect("the transport must apply the configured header timeout");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a server that never sends headers must fail"),
        };

        assert!(matches!(error, ProviderError::Transient { .. }));
        let cause = error.source().expect("header timeout cause").to_string();
        assert!(cause.contains("response headers"), "{cause}");
        assert!(cause.contains("75ms"), "{cause}");

        server.abort();
        let _ = server.await;
    }

    #[test]
    fn production_transport_installs_a_sane_default_idle_timeout() {
        assert!(
            DEFAULT_RESPONSE_IDLE_TIMEOUT >= Duration::from_secs(90),
            "the production default must accommodate established reasoning gaps"
        );
        assert!(
            DEFAULT_RESPONSE_IDLE_TIMEOUT < Duration::from_secs(200),
            "the production default must terminate before the liveness probe"
        );

        let transport = ReqwestTransport::new("default-fixture");
        assert_eq!(
            transport.idle,
            bounded_response_idle_timeout(StreamIdleTimeout::from_config(
                DEFAULT_RESPONSE_IDLE_TIMEOUT,
            )),
            "ReqwestTransport::new must install the bounded production configuration"
        );
    }

    #[test]
    fn an_excessive_idle_timeout_override_cannot_restore_the_hang() {
        let excessive = StreamIdleTimeout::new(Duration::from_secs(86_400));
        let transport = ReqwestTransport::new("override-fixture").with_idle_timeout(excessive);

        assert_eq!(transport.idle.duration(), MAX_RESPONSE_IDLE_TIMEOUT);
        assert!(MAX_RESPONSE_IDLE_TIMEOUT < Duration::from_secs(200));
    }

    #[tokio::test]
    async fn a_slow_stream_that_keeps_progressing_outlives_one_idle_window() {
        let interval = Duration::from_millis(200);
        let idle = Duration::from_millis(500);
        let (address, server) = spawn_chunked_server(
            vec![
                (Duration::ZERO, b"SLOW_"),
                (interval, b"BUT_"),
                (interval, b"STILL_"),
                (interval, b"MOVING"),
            ],
            true,
        )
        .await;
        let transport =
            ReqwestTransport::new("slow-fixture").with_idle_timeout(StreamIdleTimeout::new(idle));
        let mut chunks = transport
            .send(HttpRequest {
                url: format!("http://{address}/chat/completions"),
                headers: BTreeMap::new(),
                body: serde_json::json!({}),
                timeouts: HttpTimeouts::default(),
            })
            .await
            .expect("response headers should arrive");
        let mut received = Vec::new();

        while let Some(chunk) = chunks.next().await {
            received.extend(chunk.expect("every progressing chunk should arrive"));
        }

        server.await.expect("fixture task");
        assert_eq!(received, b"SLOW_BUT_STILL_MOVING");
        assert!(
            interval * 3 > idle,
            "the fixture must outlive one idle window"
        );
    }
}
