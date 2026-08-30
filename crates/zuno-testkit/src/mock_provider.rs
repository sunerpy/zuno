//! A loopback HTTP server that answers like a provider and records what it was asked.
//!
//! # Why capture, not just respond
//!
//! Most of the provider-compatibility work in this project is about the *outbound*
//! request: that sampling parameters are absent for reasoning models, that cache
//! breakpoints land on the right blocks, that a tool result is shaped the way the
//! provider requires. None of that is observable in a response. So every request
//! this server receives is retained in full — method, path, query, headers, body,
//! and whether streaming was asked for — and exposed for assertions through
//! [`MockProvider::captured`].
//!
//! # Where the response bytes come from
//!
//! A scenario's responses are either **recorded** — lifted out of a real cassette
//! in the oracle tree, produced by a real provider answering the real client — or
//! **authored**, meaning somebody in this repository wrote them. The distinction
//! is carried in the data as [`ResponseOrigin`], and
//! [`MockProvider::authored_scenarios`] lists the authored ones.
//!
//! That accounting exists because of a specific failure. A reference Rust agent
//! shipped an MCP stdio client that framed messages with LSP-style
//! `Content-Length` headers instead of the newline-delimited JSON the transport
//! actually uses. It was non-functional against every real MCP server, and its
//! test suite was entirely green, because the Python fixtures it validated against
//! had been written to parse the same wrong framing. Authoring a fixture is
//! sometimes necessary — an error path no real provider will produce on demand —
//! but it proves nothing about the wire format, and a harness that cannot tell the
//! two apart cannot warn anybody.
//!
//! # The invariant
//!
//! The listener binds `127.0.0.1:0`. This crate has no HTTP client in its
//! dependency graph at all, so nothing here can originate an outbound request; see
//! the note in `Cargo.toml` and `tests/no_http_client.rs`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::cassette::{Cassette, CassettePlayer, HttpInteraction};
use crate::error::{Result, TestkitError};

/// Force a specific scenario for one request, bypassing path routing.
pub const SCENARIO_HEADER: &str = "x-zuno-testkit-scenario";

/// Where a mock response's bytes came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseOrigin {
    /// Lifted from a real recorded interaction in the oracle tree.
    Recorded {
        /// The cassette name it came from.
        cassette: String,
        /// The 1-based interaction index within that cassette.
        interaction: usize,
    },
    /// Written in this repository, with a stated reason.
    ///
    /// Authored bytes are evidence about this project's expectations, never about
    /// a provider's wire format.
    Authored {
        /// Why no recording could serve this case.
        reason: String,
    },
}

impl ResponseOrigin {
    /// True when these bytes were produced by a real counterpart.
    #[must_use]
    pub fn is_recorded(&self) -> bool {
        matches!(self, Self::Recorded { .. })
    }
}

/// One response the mock will serve.
#[derive(Debug, Clone)]
pub struct MockResponse {
    /// The status code to return.
    pub status: u16,
    /// The headers to return.
    pub headers: BTreeMap<String, String>,
    /// The body bytes to return.
    pub body: Vec<u8>,
    /// Provenance of these bytes.
    pub origin: ResponseOrigin,
}

impl MockResponse {
    /// A response built from a recorded interaction.
    ///
    /// # Errors
    ///
    /// [`TestkitError::CassetteBodyEncoding`] when the recorded body claims base64
    /// and does not decode.
    pub fn from_recorded(
        cassette: &str,
        interaction_index: usize,
        interaction: &HttpInteraction,
    ) -> Result<Self> {
        Ok(Self {
            status: interaction.response.status,
            headers: interaction.response.headers.clone(),
            body: interaction
                .response
                .decoded_body(cassette, interaction_index)?,
            origin: ResponseOrigin::Recorded {
                cassette: cassette.to_owned(),
                interaction: interaction_index,
            },
        })
    }

    /// A response written here, with the reason no recording could serve it.
    #[must_use]
    pub fn authored(
        status: u16,
        content_type: &str,
        body: impl Into<Vec<u8>>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            status,
            headers: [("content-type".to_owned(), content_type.to_owned())]
                .into_iter()
                .collect(),
            body: body.into(),
            origin: ResponseOrigin::Authored {
                reason: reason.into(),
            },
        }
    }
}

/// How the harness concluded a request wanted streaming.
///
/// Kept as two separate observations rather than one boolean so a consumer can
/// assert on the specific signal a provider requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamSignal {
    /// The `stream` field in a JSON body, when present and boolean.
    pub body_flag: Option<bool>,
    /// Whether `accept` asked for `text/event-stream`.
    pub accept_sse: bool,
}

impl StreamSignal {
    /// True when either signal asked for a stream.
    #[must_use]
    pub fn wants_stream(&self) -> bool {
        self.body_flag == Some(true) || self.accept_sse
    }
}

/// One request the mock received, retained in full.
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    /// The HTTP method.
    pub method: String,
    /// The path, without the query string.
    pub path: String,
    /// The raw query string, when there was one.
    pub query: Option<String>,
    /// Every header, names lower-cased.
    pub headers: BTreeMap<String, String>,
    /// The body as received, lossily decoded as UTF-8.
    pub body: String,
    /// Whether streaming was requested.
    pub stream: bool,
    /// The individual streaming signals.
    pub stream_signal: StreamSignal,
    /// The scenario that served it, if one matched.
    pub scenario: Option<String>,
    /// Which response of that scenario was served, 1-based.
    pub served_index: Option<usize>,
    /// Where the served bytes came from.
    pub served_origin: Option<ResponseOrigin>,
}

impl CapturedRequest {
    /// The body parsed as JSON, when it is JSON.
    #[must_use]
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.body).ok()
    }

    /// A header value by lower-case name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// A named sequence of responses, optionally bound to a request path.
#[derive(Debug, Clone)]
pub struct Scenario {
    name: String,
    path: Option<String>,
    responses: Vec<MockResponse>,
}

impl Scenario {
    /// A scenario with no responses yet.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: None,
            responses: Vec::new(),
        }
    }

    /// Route requests for `path` to this scenario.
    #[must_use]
    pub fn on_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Append a response.
    #[must_use]
    pub fn respond(mut self, response: MockResponse) -> Self {
        self.responses.push(response);
        self
    }

    /// Append every interaction of a cassette, in recorded order, and bind this
    /// scenario to the path the recording was made against.
    ///
    /// # Errors
    ///
    /// The decode errors of [`MockResponse::from_recorded`].
    pub fn from_cassette(mut self, cassette_name: &str, cassette: &Cassette) -> Result<Self> {
        for (index, interaction) in cassette.http_interactions().enumerate() {
            if self.path.is_none() {
                self.path = request_path(&interaction.request.url);
            }
            self.responses.push(MockResponse::from_recorded(
                cassette_name,
                index + 1,
                interaction,
            )?);
        }
        Ok(self)
    }

    /// Load a cassette from the oracle tree and append all of its interactions.
    ///
    /// # Errors
    ///
    /// The load errors of [`CassettePlayer::from_oracle`] and the decode errors of
    /// [`MockResponse::from_recorded`].
    pub fn from_oracle_cassette(self, cassette_name: &str) -> Result<Self> {
        let player = CassettePlayer::from_oracle(cassette_name)?;
        let cassette = player.cassette().clone();
        self.from_cassette(cassette_name, &cassette)
    }

    /// This scenario's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How many responses it will serve before it is exhausted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.responses.len()
    }

    /// True when it has no responses.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.responses.is_empty()
    }
}

/// Extract the path component of an absolute URL without a URL parser.
fn request_path(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let (_, rest) = after_scheme.split_once('/')?;
    let path = rest.split('?').next().unwrap_or(rest);
    Some(format!("/{path}"))
}

#[derive(Debug)]
struct Inner {
    scenarios: Vec<Scenario>,
    cursors: Mutex<BTreeMap<String, usize>>,
    captured: Mutex<Vec<CapturedRequest>>,
}

/// A loopback provider stand-in.
#[derive(Debug)]
pub struct MockProvider {
    addr: SocketAddr,
    base_url: String,
    inner: Arc<Inner>,
    join: JoinHandle<()>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MockProvider {
    /// Start a server serving `scenarios` on an ephemeral loopback port.
    ///
    /// # Errors
    ///
    /// [`TestkitError::DuplicateScenario`] for a repeated name, or
    /// [`TestkitError::MockBind`] when the listener cannot bind.
    pub async fn start(scenarios: Vec<Scenario>) -> Result<Self> {
        let mut seen = std::collections::BTreeSet::new();
        for scenario in &scenarios {
            if !seen.insert(scenario.name.clone()) {
                return Err(TestkitError::DuplicateScenario {
                    name: scenario.name.clone(),
                });
            }
        }

        let inner = Arc::new(Inner {
            scenarios,
            cursors: Mutex::new(BTreeMap::new()),
            captured: Mutex::new(Vec::new()),
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|source| TestkitError::MockBind {
                addr: "127.0.0.1:0".to_owned(),
                source,
            })?;
        let addr = listener
            .local_addr()
            .map_err(|source| TestkitError::MockBind {
                addr: "127.0.0.1:0".to_owned(),
                source,
            })?;

        let router = axum::Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&inner));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });

        Ok(Self {
            addr,
            base_url: format!("http://{addr}"),
            inner,
            join,
            shutdown: Some(tx),
        })
    }

    /// The base URL a client should be pointed at. Always loopback.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The bound address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every request received so far, in arrival order.
    pub async fn captured(&self) -> Vec<CapturedRequest> {
        self.inner.captured.lock().await.clone()
    }

    /// How many requests have been received.
    pub async fn captured_count(&self) -> usize {
        self.inner.captured.lock().await.len()
    }

    /// The most recent request, if any.
    pub async fn last_captured(&self) -> Option<CapturedRequest> {
        self.inner.captured.lock().await.last().cloned()
    }

    /// Every registered scenario name.
    #[must_use]
    pub fn scenario_names(&self) -> Vec<&str> {
        self.inner
            .scenarios
            .iter()
            .map(|s| s.name.as_str())
            .collect()
    }

    /// Scenario names that serve at least one authored response, with the stated
    /// reasons.
    ///
    /// A consumer validating a wire format should assert this is empty for the
    /// scenarios it relies on.
    #[must_use]
    pub fn authored_scenarios(&self) -> Vec<(&str, Vec<&str>)> {
        self.inner
            .scenarios
            .iter()
            .filter_map(|s| {
                let reasons: Vec<&str> = s
                    .responses
                    .iter()
                    .filter_map(|r| match &r.origin {
                        ResponseOrigin::Authored { reason } => Some(reason.as_str()),
                        ResponseOrigin::Recorded { .. } => None,
                    })
                    .collect();
                (!reasons.is_empty()).then_some((s.name.as_str(), reasons))
            })
            .collect()
    }

    /// Stop the server and wait for it to finish.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = (&mut self.join).await;
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.join.abort();
    }
}

/// Every request is captured before any response is chosen, so a request that
/// matches nothing is still visible to the test that made it.
async fn handle(State(inner): State<Arc<Inner>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&bytes).into_owned();

    let mut headers = BTreeMap::new();
    for (name, value) in &parts.headers {
        headers.insert(
            name.as_str().to_ascii_lowercase(),
            value.to_str().unwrap_or("<non-utf8>").to_owned(),
        );
    }

    let stream_signal = StreamSignal {
        body_flag: serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("stream").and_then(serde_json::Value::as_bool)),
        accept_sse: headers
            .get("accept")
            .is_some_and(|a| a.contains("text/event-stream")),
    };

    let path = parts.uri.path().to_owned();
    let mut captured = CapturedRequest {
        method: parts.method.as_str().to_owned(),
        path: path.clone(),
        query: parts.uri.query().map(str::to_owned),
        stream: stream_signal.wants_stream(),
        stream_signal,
        scenario: None,
        served_index: None,
        served_origin: None,
        headers,
        body,
    };

    let selected = select_scenario(&inner, &captured, &path);
    let response = match selected {
        None => {
            let names = inner
                .scenarios
                .iter()
                .map(|s| {
                    s.path
                        .as_deref()
                        .map_or_else(|| s.name.clone(), |p| format!("{} on {p}", s.name))
                })
                .collect::<Vec<_>>()
                .join(", ");
            diagnostic(
                404,
                &format!("no scenario matched {path}; registered: [{names}]"),
            )
        }
        Some(index) => {
            let scenario = &inner.scenarios[index];
            captured.scenario = Some(scenario.name.clone());
            let mut cursors = inner.cursors.lock().await;
            let cursor = cursors.entry(scenario.name.clone()).or_insert(0);
            match scenario.responses.get(*cursor) {
                None => diagnostic(
                    500,
                    &format!(
                        "scenario {:?} is exhausted: {} response(s) recorded, request {} received",
                        scenario.name,
                        scenario.responses.len(),
                        *cursor + 1
                    ),
                ),
                Some(mock) => {
                    captured.served_index = Some(*cursor + 1);
                    captured.served_origin = Some(mock.origin.clone());
                    *cursor += 1;
                    build_response(mock)
                }
            }
        }
    };

    inner.captured.lock().await.push(captured);
    response
}

fn select_scenario(inner: &Inner, captured: &CapturedRequest, path: &str) -> Option<usize> {
    if let Some(forced) = captured.headers.get(SCENARIO_HEADER) {
        return inner.scenarios.iter().position(|s| s.name == *forced);
    }
    if let Some(index) = inner
        .scenarios
        .iter()
        .position(|s| s.path.as_deref() == Some(path))
    {
        return Some(index);
    }
    (inner.scenarios.len() == 1).then_some(0)
}

fn build_response(mock: &MockResponse) -> Response {
    let mut builder = Response::builder().status(mock.status);
    for (name, value) in &mock.headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(mock.body.clone()))
        .unwrap_or_else(|_| diagnostic(500, "recorded response headers are not valid HTTP"))
}

/// A diagnostic reply the harness itself produced, distinguishable from anything a
/// provider would send by its `x-zuno-testkit-diagnostic` header.
fn diagnostic(status: u16, message: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .header("x-zuno-testkit-diagnostic", "1")
        .body(Body::from(message.to_owned()))
        .unwrap_or_else(|_| Response::new(Body::from(message.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// A hand-written HTTP/1.1 exchange over a raw socket.
    ///
    /// The self-tests deliberately do not use a production client. A mock proved
    /// correct by the same client that will later be under test is the exact shape
    /// of the `Content-Length`-framing failure described in the module docs: two
    /// components agreeing because they share one assumption. Writing the bytes by
    /// hand means the assertion is about bytes.
    async fn raw_post(
        addr: SocketAddr,
        path: &str,
        extra_headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, BTreeMap<String, String>, String) {
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let mut request = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in extra_headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).await.expect("write");
        stream.flush().await.expect("flush");

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.expect("read");
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, response_body) = text.split_once("\r\n\r\n").expect("a header terminator");
        let mut lines = head.lines();
        let status: u16 = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .expect("a status line");
        let headers = lines
            .filter_map(|l| l.split_once(": "))
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_owned()))
            .collect();
        (status, headers, response_body.to_owned())
    }

    #[tokio::test]
    async fn the_listener_is_always_loopback() {
        let mock = MockProvider::start(vec![]).await.expect("start");
        assert!(mock.addr().ip().is_loopback(), "{}", mock.addr());
        assert!(
            mock.base_url().starts_with("http://127.0.0.1:"),
            "{}",
            mock.base_url()
        );
        mock.shutdown().await;
    }

    /// The round-trip acceptance criterion: what the client sent is what the test
    /// can assert on.
    #[tokio::test]
    async fn a_request_round_trips_into_a_captured_record() {
        if crate::recordings_root_or_skip(
            "a_request_round_trips_into_a_captured_record",
            "recorded mock-provider round trip was NOT tested",
        )
        .is_none()
        {
            return;
        }
        let scenario = Scenario::new("anthropic-text")
            .from_oracle_cassette("anthropic-messages/streams-text")
            .expect("the pinned corpus contains this recording");
        let recorded_path = scenario.path.clone();
        assert_eq!(recorded_path.as_deref(), Some("/v1/messages"));

        let mock = MockProvider::start(vec![scenario]).await.expect("start");
        let sent = r#"{"model":"claude-haiku-4-5-20251001","stream":true,"max_tokens":20}"#;
        let (status, headers, body) = raw_post(
            mock.addr(),
            "/v1/messages?beta=true",
            &[
                ("content-type", "application/json"),
                ("anthropic-version", "2023-06-01"),
                ("x-api-key", "sk-test-not-a-real-key"),
            ],
            sent,
        )
        .await;

        assert_eq!(status, 200);
        assert_eq!(
            headers.get("content-type").map(String::as_str),
            Some("text/event-stream; charset=utf-8"),
            "the recorded content type must be served verbatim"
        );
        assert!(body.contains("event: message_start"), "{body}");

        let captured = mock.captured().await;
        assert_eq!(captured.len(), 1);
        let request = &captured[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(request.query.as_deref(), Some("beta=true"));
        assert_eq!(request.body, sent, "the body must be captured verbatim");
        assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(request.header("x-api-key"), Some("sk-test-not-a-real-key"));
        assert!(request.stream);
        assert_eq!(request.stream_signal.body_flag, Some(true));
        assert!(!request.stream_signal.accept_sse);
        assert_eq!(request.scenario.as_deref(), Some("anthropic-text"));
        assert_eq!(request.served_index, Some(1));
        assert_eq!(
            request.served_origin,
            Some(ResponseOrigin::Recorded {
                cassette: "anthropic-messages/streams-text".to_owned(),
                interaction: 1,
            }),
            "a response lifted from a recording must say so"
        );
        assert_eq!(
            request
                .json()
                .and_then(|v| v.get("max_tokens").and_then(serde_json::Value::as_u64)),
            Some(20)
        );
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn a_non_streaming_request_is_captured_as_non_streaming() {
        let mock =
            MockProvider::start(vec![Scenario::new("only").respond(MockResponse::authored(
                200,
                "application/json",
                r#"{"ok":true}"#,
                "exercises the non-streaming capture path",
            ))])
            .await
            .expect("start");
        raw_post(
            mock.addr(),
            "/v1/messages",
            &[("content-type", "application/json")],
            r#"{"stream":false}"#,
        )
        .await;
        let captured = mock.last_captured().await.expect("one request");
        assert!(!captured.stream);
        assert_eq!(captured.stream_signal.body_flag, Some(false));
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn an_accept_header_alone_signals_streaming() {
        let mock = MockProvider::start(vec![Scenario::new("only").respond(
            MockResponse::authored(200, "application/json", "{}", "capture path"),
        )])
        .await
        .expect("start");
        raw_post(
            mock.addr(),
            "/v1/x",
            &[("accept", "text/event-stream")],
            "not json",
        )
        .await;
        let captured = mock.last_captured().await.expect("one request");
        assert!(captured.stream);
        assert_eq!(captured.stream_signal.body_flag, None);
        assert!(captured.stream_signal.accept_sse);
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn multiple_responses_are_served_in_recorded_order() {
        if crate::recordings_root_or_skip(
            "multiple_responses_are_served_in_recorded_order",
            "recorded multi-response ordering was NOT tested",
        )
        .is_none()
        {
            return;
        }
        let scenario = Scenario::new("tool-loop")
            .from_oracle_cassette("anthropic-messages/claude-opus-4-7-drives-a-tool-loop")
            .expect("recording");
        assert_eq!(scenario.len(), 2);
        let mock = MockProvider::start(vec![scenario]).await.expect("start");
        let (_, _, first) = raw_post(mock.addr(), "/v1/messages", &[], "{}").await;
        let (_, _, second) = raw_post(mock.addr(), "/v1/messages", &[], "{}").await;
        assert_ne!(first, second, "the two recorded turns differ");
        let captured = mock.captured().await;
        assert_eq!(captured[0].served_index, Some(1));
        assert_eq!(captured[1].served_index, Some(2));
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn exhaustion_is_a_diagnostic_and_the_request_is_still_captured() {
        let mock = MockProvider::start(vec![Scenario::new("one").respond(MockResponse::authored(
            200,
            "application/json",
            "{}",
            "single turn",
        ))])
        .await
        .expect("start");
        raw_post(mock.addr(), "/v1/x", &[], "{}").await;
        let (status, headers, body) = raw_post(mock.addr(), "/v1/x", &[], "{}").await;
        assert_eq!(status, 500);
        assert_eq!(
            headers.get("x-zuno-testkit-diagnostic").map(String::as_str),
            Some("1")
        );
        assert!(body.contains("exhausted"), "{body}");
        assert_eq!(
            mock.captured_count().await,
            2,
            "an unanswerable request is still evidence"
        );
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn an_unmatched_path_names_the_registered_scenarios() {
        let mock = MockProvider::start(vec![
            Scenario::new("a")
                .on_path("/v1/messages")
                .respond(MockResponse::authored(200, "application/json", "{}", "x")),
            Scenario::new("b")
                .on_path("/v1/responses")
                .respond(MockResponse::authored(200, "application/json", "{}", "x")),
        ])
        .await
        .expect("start");
        let (status, _, body) = raw_post(mock.addr(), "/v1/nowhere", &[], "{}").await;
        assert_eq!(status, 404);
        assert!(body.contains("/v1/messages"), "{body}");
        assert!(body.contains("/v1/responses"), "{body}");
        let captured = mock.last_captured().await.expect("still captured");
        assert!(captured.scenario.is_none());
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn the_scenario_header_overrides_path_routing() {
        let mock = MockProvider::start(vec![
            Scenario::new("recorded")
                .on_path("/v1/messages")
                .respond(MockResponse::authored(
                    200,
                    "application/json",
                    r#"{"which":"recorded"}"#,
                    "x",
                )),
            Scenario::new("refusal")
                .on_path("/v1/messages")
                .respond(MockResponse::authored(
                    400,
                    "application/json",
                    r#"{"which":"refusal"}"#,
                    "no real provider refuses on demand",
                )),
        ])
        .await
        .expect("start");
        let (status, _, body) = raw_post(
            mock.addr(),
            "/v1/messages",
            &[(SCENARIO_HEADER, "refusal")],
            "{}",
        )
        .await;
        assert_eq!(status, 400);
        assert!(body.contains("refusal"), "{body}");
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn authored_responses_are_declared_and_recorded_ones_are_not() {
        if crate::recordings_root_or_skip(
            "authored_responses_are_declared_and_recorded_ones_are_not",
            "recorded-versus-authored provenance was NOT tested",
        )
        .is_none()
        {
            return;
        }
        let recorded = Scenario::new("recorded")
            .from_oracle_cassette("anthropic-messages/streams-text")
            .expect("recording");
        let authored = Scenario::new("refusal").respond(MockResponse::authored(
            400,
            "application/json",
            r#"{"error":"x"}"#,
            "no real provider will refuse on demand",
        ));
        let mock = MockProvider::start(vec![recorded, authored])
            .await
            .expect("start");
        let declared = mock.authored_scenarios();
        assert_eq!(declared.len(), 1, "only the authored scenario is declared");
        assert_eq!(declared[0].0, "refusal");
        assert_eq!(
            declared[0].1,
            vec!["no real provider will refuse on demand"]
        );
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn a_duplicate_scenario_name_is_refused() {
        let err = MockProvider::start(vec![Scenario::new("dup"), Scenario::new("dup")])
            .await
            .expect_err("names must be unique");
        assert!(
            matches!(err, TestkitError::DuplicateScenario { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn the_recorded_url_yields_the_path_a_client_will_hit() {
        assert_eq!(
            request_path("https://api.anthropic.com/v1/messages").as_deref(),
            Some("/v1/messages")
        );
        assert_eq!(
            request_path(
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
            )
            .as_deref(),
            Some("/v1beta/models/gemini-2.5-flash:streamGenerateContent")
        );
        assert_eq!(request_path("https://api.openai.com").as_deref(), None);
    }
}
