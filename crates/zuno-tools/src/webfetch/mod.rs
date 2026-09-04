//! `webfetch`: retrieve a URL and hand its content back as text, markdown or HTML.
//!
//! # The three bounds
//!
//! Every fetch is bounded in time, size and redirect hops. See
//! [`bounds`] for the values, where each came from, and why a missing
//! bound on any one of them is a distinct production failure.
//!
//! # Registry key versus wire id
//!
//! Upstream registers this tool under the object key `fetch`
//! (`packages/opencode/src/tool/registry.ts:216`, `fetch: Tool.init(webfetch)`) while
//! the id the model calls is `webfetch` (`Tool.define("webfetch", …)`,
//! `webfetch.ts:24`). [`Tool::id`](zuno_tool::Tool::id) is the wire id, because that is
//! the name a model emits and the name the permission layer keys on
//! (`zuno-config`'s `KNOWN_KEYS` lists `webfetch`). The `fetch` key is an internal
//! handle in upstream's registry record and has no wire meaning; todo 44 owns
//! whether to reproduce it.
//!
//! # Fetched content is data, not instruction
//!
//! Everything this tool returns came from a stranger. It is placed in
//! [`ToolOutput::output`] as ordinary text and given no structural privilege: the
//! converters in [`html`] decide every heading, fence and delimiter, so a page
//! cannot forge document structure, and a page that says "ignore previous
//! instructions" is returned as a paragraph saying that.

pub mod body;
pub mod bounds;
pub mod html;

use async_trait::async_trait;
use bounds::{MAX_REDIRECTS, MAX_RESPONSE_BYTES, WebError, resolve_timeout};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zuno_error::ToolError;
use zuno_network::{
    DiagnosticEndpoint, PublicHttpClient, PublicHttpPolicy, PublicHttpResponse, PublicTarget,
};
use zuno_tool::{Attachment, ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy, TypedTool};

/// The wire id, and the permission key.
pub const ID: &str = "webfetch";

/// The description the model reads.
///
/// Verbatim from `packages/opencode/src/tool/webfetch.txt`, because the description
/// is a compatibility surface: a model tuned against upstream's wording should read
/// the same wording here.
///
/// # The upgrade line is upstream's, and upstream does not honour it
///
/// "HTTP URLs will be automatically upgraded to HTTPS" is false in every upstream
/// implementation. `packages/opencode/src/tool/webfetch.ts:34-36` only checks the
/// prefix, and `packages/core/src/tool/webfetch.ts:82-84` (`assertHttpUrl`) only
/// checks the protocol; neither rewrites the scheme, and the request is issued
/// against the URL as given. This port reproduces the behaviour, not the claim — see
/// [`parse_target`] — and keeps the text byte-identical rather than silently shipping
/// a different description than upstream does.
pub const DESCRIPTION: &str = include_str!("../description/webfetch.txt");

/// The browser user agent sent first.
///
/// Oracle: `packages/core/src/tool/webfetch.ts:64-65`.
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

/// The user agent used on the Cloudflare-challenge retry.
///
const HONEST_USER_AGENT: &str = "zuno";

/// The requested representation of the fetched document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// Plain text, with markup stripped.
    Text,
    /// Markdown, converted from HTML when the response is HTML.
    #[default]
    Markdown,
    /// The response body unchanged.
    Html,
}

impl Format {
    /// The `Accept` header for this format, with the same fallback weights upstream
    /// sends (`packages/core/src/tool/webfetch.ts:47-56`).
    const fn accept(self) -> &'static str {
        match self {
            Self::Markdown => {
                "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
            }
            Self::Text => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
            Self::Html => {
                "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1"
            }
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Markdown => "markdown",
            Self::Html => "html",
        }
    }
}

/// The arguments, deserialized and schema-derived from one declaration.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebFetchParams {
    /// The URL to fetch content from
    pub url: String,
    /// The format to return the content in (text, markdown, or html). Defaults to markdown.
    #[serde(default)]
    pub format: Format,
    /// Optional timeout in seconds (max 120)
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// Fetches a URL under a time, size and redirect budget.
pub struct WebFetchTool {
    transport: WebFetchTransport,
}

enum WebFetchTransport {
    Public(std::sync::Arc<PublicHttpClient>),
    Raw(reqwest::Client),
}

impl WebFetchTransport {
    /// This transport with its connection-establishment budget derived from `budget`,
    /// the caller's whole allowance for one call.
    ///
    /// The shared client carries the 10-second per-address default, which is unrelated
    /// to what the caller can afford: a `timeout: 5` call against a host whose first
    /// address completes TCP and then goes silent spent all five seconds on that
    /// address and never reached the healthy second one — an outcome the owner of any
    /// DNS zone the model is told to read can produce on demand. One address may now
    /// spend a third of the budget, so a second and a third address are still reachable
    /// inside it; `with_establish_timeout` clamps into `[100ms, 10s]`, so a caller can
    /// only tighten the shared default, never widen it, and the clone is cheap — the TLS
    /// configuration and the resolver are shared, only the `Duration` differs.
    fn for_budget(&self, budget: Duration) -> Self {
        match self {
            Self::Public(client) => Self::Public(std::sync::Arc::new(
                PublicHttpClient::clone(client).with_establish_timeout(establish_share(budget)),
            )),
            Self::Raw(client) => Self::Raw(client.clone()),
        }
    }
}

/// The share of a call's budget one address may spend establishing a connection.
///
/// A third, so that when the first validated address stalls the client still has two
/// more attempts inside the same budget rather than a timeout with nothing tried.
fn establish_share(budget: Duration) -> Duration {
    budget / 3
}

enum FetchResponse {
    Public(PublicHttpResponse),
    Raw(reqwest::Response),
}

impl FetchResponse {
    fn status(&self) -> reqwest::StatusCode {
        match self {
            Self::Public(response) => response.status(),
            Self::Raw(response) => response.status(),
        }
    }

    fn headers(&self) -> &reqwest::header::HeaderMap {
        match self {
            Self::Public(response) => response.headers(),
            Self::Raw(response) => response.headers(),
        }
    }

    fn route(&self) -> &'static str {
        match self {
            Self::Public(response) => response.route(),
            Self::Raw(_) => "direct_test",
        }
    }
}

#[derive(Clone)]
struct FetchProgress {
    route: Arc<Mutex<String>>,
    phase: Arc<AtomicU8>,
}

impl FetchProgress {
    const RESOLVE_CONNECT: u8 = 0;
    const RESPONSE_BODY: u8 = 1;
    const CONVERT: u8 = 2;

    fn new(route: String) -> Self {
        Self {
            route: Arc::new(Mutex::new(route)),
            phase: Arc::new(AtomicU8::new(Self::RESOLVE_CONNECT)),
        }
    }

    fn set_route(&self, route: &str) {
        *self
            .route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = route.to_owned();
    }

    fn set_phase(&self, phase: u8) {
        self.phase.store(phase, Ordering::Release);
    }

    fn route(&self) -> String {
        self.route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn phase(&self) -> &'static str {
        match self.phase.load(Ordering::Acquire) {
            Self::RESOLVE_CONNECT => "resolve_connect",
            Self::RESPONSE_BODY => "response_body",
            Self::CONVERT => "convert",
            _ => "unknown",
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    /// A tool using the shared public-internet transport.
    #[must_use]
    pub fn new() -> Self {
        Self {
            transport: WebFetchTransport::Public(std::sync::Arc::new(
                PublicHttpClient::with_resolver(
                    std::sync::Arc::new(zuno_network::SystemHostResolver),
                    PublicHttpPolicy {
                        max_redirects: MAX_REDIRECTS,
                    },
                ),
            )),
        }
    }

    /// Build the tool from the public transport activated by the current profile.
    #[must_use]
    pub fn with_public_client(client: std::sync::Arc<PublicHttpClient>) -> Self {
        Self {
            transport: WebFetchTransport::Public(client),
        }
    }

    /// A raw-client seam for loopback-only integration tests.
    ///
    /// Production profiles assemble [`Self::new`]. A caller using this constructor owns
    /// target validation and must never expose it as the shipped `webfetch` capability.
    #[must_use]
    pub const fn with_client(client: reqwest::Client) -> Self {
        Self {
            transport: WebFetchTransport::Raw(client),
        }
    }

    /// Issues the request, retrying once with an honest user agent when Cloudflare
    /// answers the browser user agent with a challenge.
    ///
    /// Oracle: `packages/opencode/src/tool/webfetch.ts:78-88`. The retry exists
    /// because the browser user agent does not match this client's TLS fingerprint,
    /// so claiming to be Chrome is what triggers the block.
    async fn send(
        transport: &WebFetchTransport,
        target: &PublicTarget,
        format: Format,
        ctx: &ToolContext,
    ) -> Result<FetchResponse, WebError> {
        let first = Self::get(transport, target, format, BROWSER_USER_AGENT, ctx).await?;
        if is_cloudflare_challenge(&first) {
            return Self::get(transport, target, format, HONEST_USER_AGENT, ctx).await;
        }
        Ok(first)
    }

    async fn get(
        transport: &WebFetchTransport,
        target: &PublicTarget,
        format: Format,
        user_agent: &str,
        ctx: &ToolContext,
    ) -> Result<FetchResponse, WebError> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_str(user_agent)
                .expect("the static user agent is a valid header"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static(format.accept()),
        );
        headers.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            reqwest::header::HeaderValue::from_static("en-US,en;q=0.9"),
        );
        let request = async {
            match transport {
                WebFetchTransport::Public(client) => client
                    .get(target.clone(), headers)
                    .await
                    .map(FetchResponse::Public)
                    .map_err(WebError::from),
                WebFetchTransport::Raw(client) => client
                    .get(target.url().clone())
                    .headers(headers)
                    .send()
                    .await
                    .map(FetchResponse::Raw)
                    .map_err(|source| classify_send_error(&target.diagnostic(), source)),
            }
        };
        tokio::select! {
            biased;
            () = ctx.interrupt.notified() => Err(WebError::Interrupted {
                read: 0,
            }),
            result = request => result,
        }
    }

    /// Everything after argument validation and permission, under one time budget.
    ///
    /// `transport` is the per-call transport from [`WebFetchTransport::for_budget`], not
    /// the shared one, so the establishment budget it carries is the caller's.
    async fn fetch(
        transport: &WebFetchTransport,
        params: &WebFetchParams,
        target: &PublicTarget,
        ctx: &ToolContext,
        progress: &FetchProgress,
    ) -> Result<ToolOutput, WebError> {
        let response = Self::send(transport, target, params.format, ctx).await?;
        progress.set_route(response.route());

        let status = response.status();
        if !status.is_success() {
            return Err(WebError::Status {
                url: target.diagnostic().to_string(),
                status: status.as_u16(),
                retry_after: bounds::retry_after(response.headers()),
            });
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let mime = mime_of(&content_type);

        progress.set_phase(FetchProgress::RESPONSE_BODY);
        let body = match response {
            FetchResponse::Public(response) => {
                body::read_public_bounded(response, MAX_RESPONSE_BYTES, ctx.interrupt.as_ref())
                    .await?
            }
            FetchResponse::Raw(response) => {
                body::read_bounded(response, MAX_RESPONSE_BYTES, ctx.interrupt.as_ref()).await?
            }
        };
        progress.set_phase(FetchProgress::CONVERT);
        let title = format!("{} ({content_type})", params.url);

        if is_image_attachment(&mime) {
            // Oracle: `packages/opencode/src/tool/webfetch.ts:106-121` returns the
            // image as a data-URL attachment rather than decoding bytes as text.
            return Ok(ToolOutput::text(title, "Image fetched successfully")
                .with_metadata("url", params.url.clone())
                .with_metadata("contentType", content_type.clone())
                .with_attachment(Attachment::new(
                    mime.clone(),
                    format!("data:{mime};base64,{}", base64(&body)),
                )));
        }

        let content = String::from_utf8_lossy(&body);
        let output = convert(&content, &content_type, params.format);

        Ok(ToolOutput::text(title, output)
            .with_metadata("url", params.url.clone())
            .with_metadata("contentType", content_type)
            .with_metadata("format", params.format.as_str()))
    }
}

#[async_trait]
impl TypedTool for WebFetchTool {
    type Params = WebFetchParams;

    fn id(&self) -> &str {
        ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn effect(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn run(&self, params: WebFetchParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let target = parse_target(&params.url).map_err(invalid_args)?;

        ctx.ask(
            ID,
            zuno_tool::PermissionAsk {
                permission: ID.to_owned(),
                patterns: vec![params.url.clone()],
                metadata: metadata(&params),
                always: vec!["*".to_owned()],
                ..zuno_tool::PermissionAsk::default()
            },
        )
        .await?;

        let budget = resolve_timeout(params.timeout);
        let transport = self.transport.for_budget(budget);
        let initial_route = match &transport {
            WebFetchTransport::Public(client) => client
                .route_label(&target)
                .unwrap_or("proxy_configuration")
                .to_owned(),
            WebFetchTransport::Raw(_) => "direct_test".to_owned(),
        };
        let progress = FetchProgress::new(initial_route);
        match tokio::time::timeout(
            budget,
            Self::fetch(&transport, &params, &target, &ctx, &progress),
        )
        .await
        {
            Ok(result) => result.map_err(failed),
            Err(_elapsed) => Err(ToolError::NetworkTimeout {
                tool: ID.to_owned(),
                route: progress.route(),
                phase: progress.phase(),
                elapsed: budget,
            }),
        }
    }
}

/// Validates the URL and returns it parsed.
///
/// Rejects anything that is not `http` or `https` — a `file://` or `data:` URL here
/// would turn a web tool into an unbounded local read.
///
/// Does **not** rewrite `http` to `https`, despite [`DESCRIPTION`] saying so; see
/// that constant for why the claim is upstream's and the behaviour is upstream's too.
///
/// # Errors
/// [`WebError::MalformedUrl`] when the input does not parse, or
/// [`WebError::UnsupportedScheme`] when it parses to a non-HTTP scheme.
pub fn parse_target(url: &str) -> Result<PublicTarget, WebError> {
    match PublicTarget::parse(url) {
        Ok(target) => Ok(target),
        Err(zuno_network::PublicHttpError::MalformedUrl { source }) => {
            Err(WebError::MalformedUrl {
                url: url.to_owned(),
                source,
            })
        }
        Err(zuno_network::PublicHttpError::UnsupportedScheme { .. }) => {
            Err(WebError::UnsupportedScheme {
                url: url.to_owned(),
            })
        }
        Err(source) => Err(WebError::PublicTarget { source }),
    }
}

/// Applies the requested conversion, which only ever engages for HTML.
///
/// Oracle: `packages/core/src/tool/webfetch.ts:110-115` — a non-HTML body is handed
/// back unchanged whatever the requested format, because there is no markup to
/// convert and inventing one would corrupt JSON, CSV or plain text.
fn convert(content: &str, content_type: &str, format: Format) -> String {
    if !content_type.contains("text/html") {
        return content.to_owned();
    }
    match format {
        Format::Markdown => html::to_markdown(content),
        Format::Text => html::to_text(content),
        Format::Html => content.to_owned(),
    }
}

fn metadata(params: &WebFetchParams) -> serde_json::Map<String, serde_json::Value> {
    let mut metadata = serde_json::Map::new();
    metadata.insert("url".to_owned(), json!(params.url));
    metadata.insert("format".to_owned(), json!(params.format.as_str()));
    metadata.insert("timeout".to_owned(), json!(params.timeout));
    metadata
}

/// The media type without its parameters, lowercased.
fn mime_of(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Whether a media type is an image this tool returns as an attachment.
///
/// Oracle: `packages/core/src/tool/webfetch.ts:101-102`. SVG is excluded because it
/// is markup a model can read, not a raster the model needs decoded.
#[must_use]
pub fn is_image_attachment(mime: &str) -> bool {
    mime.starts_with("image/") && mime != "image/svg+xml" && mime != "image/vnd.fastbidsheet"
}

/// Cloudflare's bot-challenge signature.
///
/// Oracle: `packages/core/src/tool/webfetch.ts:78-79`.
fn is_cloudflare_challenge(response: &FetchResponse) -> bool {
    response.status() == reqwest::StatusCode::FORBIDDEN
        && response
            .headers()
            .get("cf-mitigated")
            .and_then(|value| value.to_str().ok())
            == Some("challenge")
}

/// Separates a redirect-cap failure from every other transport failure.
///
/// `reqwest` reports an exhausted redirect policy as an ordinary request error, so
/// without this the hop cap would be indistinguishable from a refused connection and
/// the abandoned-chain message would never be seen.
fn classify_send_error(endpoint: &DiagnosticEndpoint, source: reqwest::Error) -> WebError {
    if source.is_redirect() {
        return WebError::TooManyRedirects {
            limit: MAX_REDIRECTS,
        };
    }
    WebError::Transport {
        url: endpoint.to_string(),
        source: source.without_url(),
    }
}

fn invalid_args(error: WebError) -> ToolError {
    ToolError::InvalidArgs {
        tool: ID.to_owned(),
        source: Box::new(error),
    }
}

fn failed(error: WebError) -> ToolError {
    let retry_after = error.retry_after();
    if error.is_transient() {
        ToolError::Transient {
            tool: ID.to_owned(),
            retry_after,
            source: Box::new(error),
        }
    } else {
        ToolError::Failed {
            tool: ID.to_owned(),
            source: Box::new(error),
        }
    }
}

/// Standard base64, for the image data URL.
///
/// Hand-rolled rather than adding a dependency for 20 lines; the alphabet and
/// padding are RFC 4648 §4.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }

    out
}

/// The overall time budget a set of parameters resolves to, for callers that need to
/// reason about the deadline without running the tool.
#[must_use]
pub fn budget(params: &WebFetchParams) -> Duration {
    resolve_timeout(params.timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_id_is_webfetch_not_the_registry_key_fetch() {
        assert_eq!(WebFetchTool::new().id(), "webfetch");
    }

    #[test]
    fn a_non_http_scheme_is_refused() {
        let error = parse_target("file:///etc/passwd").expect_err("file:// must be refused");
        assert!(
            matches!(error, WebError::UnsupportedScheme { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_data_url_is_refused() {
        assert!(parse_target("data:text/html,<p>x</p>").is_err());
    }

    #[test]
    fn a_non_url_is_refused_as_malformed() {
        let error = parse_target("not a url").expect_err("must be refused");
        assert!(matches!(error, WebError::MalformedUrl { .. }), "{error}");
    }

    #[test]
    fn http_is_not_rewritten_to_https() {
        // Upstream's description promises an upgrade that no upstream implementation
        // performs; reproducing the behaviour means the scheme survives untouched.
        let parsed = parse_target("http://example.test/x").expect("http is accepted");
        assert_eq!(parsed.url().scheme(), "http");
        assert_eq!(parsed.url().as_str(), "http://example.test/x");
    }

    #[test]
    fn credentials_and_private_literals_are_refused_before_permission() {
        assert!(matches!(
            parse_target("https://user:secret@example.test/path"),
            Err(WebError::PublicTarget { .. })
        ));
        assert!(matches!(
            parse_target("http://127.0.0.1/private"),
            Err(WebError::PublicTarget { .. })
        ));
    }

    #[test]
    fn the_default_format_is_markdown() {
        let params: WebFetchParams =
            serde_json::from_value(json!({ "url": "https://e.test" })).expect("defaults");
        assert_eq!(params.format, Format::Markdown);
        assert_eq!(params.timeout, None);
    }

    #[test]
    fn the_schema_is_derived_and_names_every_parameter() {
        let schema = zuno_tool::Tool::raw_parameters_schema(&zuno_tool::Typed(WebFetchTool::new()));
        let properties = schema["properties"]
            .as_object()
            .expect("an object schema with properties");
        assert!(properties.contains_key("url"));
        assert!(properties.contains_key("format"));
        assert!(properties.contains_key("timeout"));
        assert_eq!(schema["required"], json!(["url"]));
    }

    #[test]
    fn a_non_html_body_is_never_converted() {
        let json = r#"{"a": 1}"#;
        assert_eq!(convert(json, "application/json", Format::Markdown), json);
        assert_eq!(convert(json, "application/json", Format::Text), json);
    }

    #[test]
    fn svg_is_read_as_markup_not_returned_as_an_attachment() {
        assert!(!is_image_attachment("image/svg+xml"));
        assert!(is_image_attachment("image/png"));
    }

    #[test]
    fn base64_matches_rfc4648_including_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_content_type_parameters_are_stripped_from_the_mime() {
        assert_eq!(mime_of("Text/HTML; charset=UTF-8"), "text/html");
    }

    fn establish_timeout_of(transport: &WebFetchTransport) -> String {
        let WebFetchTransport::Public(client) = transport else {
            panic!("the shipped tool uses the public transport");
        };
        let rendered = format!("{client:?}");
        let start = rendered
            .find("establish_timeout: ")
            .expect("the transport reports its establishment budget");
        rendered[start + "establish_timeout: ".len()..]
            .split([',', ' ', '}'])
            .next()
            .expect("a duration follows the field name")
            .to_owned()
    }

    /// `with_establish_timeout` had a provider and tests but no production caller, so
    /// every real fetch ran with the 10-second per-address default whatever the caller's
    /// `timeout`. The per-call transport carries a third of the budget instead.
    #[test]
    fn the_establishment_budget_follows_the_callers_timeout() {
        let tool = WebFetchTool::new();
        assert_eq!(establish_timeout_of(&tool.transport), "10s");

        let five_seconds = tool.transport.for_budget(resolve_timeout(Some(5)));
        assert!(
            establish_timeout_of(&five_seconds).starts_with("1.666"),
            "a 5s call gives one address a third of it: {}",
            establish_timeout_of(&five_seconds)
        );

        let thirty_seconds = tool.transport.for_budget(resolve_timeout(None));
        assert_eq!(
            establish_timeout_of(&thirty_seconds),
            "10s",
            "the default 30s budget cannot widen the 10s shared ceiling"
        );

        let maximum = tool.transport.for_budget(resolve_timeout(Some(600)));
        assert_eq!(establish_timeout_of(&maximum), "10s");

        let one_second = tool.transport.for_budget(Duration::from_secs(1));
        assert!(
            establish_timeout_of(&one_second).starts_with("333.333"),
            "{}",
            establish_timeout_of(&one_second)
        );

        assert_eq!(
            establish_timeout_of(&tool.transport),
            "10s",
            "deriving a per-call transport leaves the shared client untouched"
        );
    }
}
