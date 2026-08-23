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
use std::time::Duration;
use zuno_error::ToolError;
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
    client: reqwest::Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    /// A tool with a client whose redirect policy is capped at [`MAX_REDIRECTS`].
    ///
    /// # Panics
    /// Never in practice: the only configuration here is the redirect policy, and
    /// `reqwest`'s builder cannot fail on it. A failure would mean the TLS backend
    /// did not initialize, which is not a condition a tool can proceed past.
    #[must_use]
    pub fn new() -> Self {
        Self::with_client(
            zuno_network::client_builder()
                .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                .build()
                .expect("reqwest client with a redirect policy"),
        )
    }

    /// A tool over a caller-supplied client, for tests that need a different policy.
    #[must_use]
    pub const fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Issues the request, retrying once with an honest user agent when Cloudflare
    /// answers the browser user agent with a challenge.
    ///
    /// Oracle: `packages/opencode/src/tool/webfetch.ts:78-88`. The retry exists
    /// because the browser user agent does not match this client's TLS fingerprint,
    /// so claiming to be Chrome is what triggers the block.
    async fn send(&self, url: &str, format: Format) -> Result<reqwest::Response, WebError> {
        let first = self.get(url, format, BROWSER_USER_AGENT).await?;
        if is_cloudflare_challenge(&first) {
            return self.get(url, format, HONEST_USER_AGENT).await;
        }
        Ok(first)
    }

    async fn get(
        &self,
        url: &str,
        format: Format,
        user_agent: &str,
    ) -> Result<reqwest::Response, WebError> {
        self.client
            .get(url)
            .header(reqwest::header::USER_AGENT, user_agent)
            .header(reqwest::header::ACCEPT, format.accept())
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .send()
            .await
            .map_err(|source| classify_send_error(url, source))
    }

    /// Everything after argument validation and permission, under one time budget.
    async fn fetch(
        &self,
        params: &WebFetchParams,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, WebError> {
        let response = self.send(&params.url, params.format).await?;

        let status = response.status();
        if !status.is_success() {
            return Err(WebError::Status {
                url: params.url.clone(),
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

        let body = body::read_bounded(response, MAX_RESPONSE_BYTES, ctx.interrupt.as_ref()).await?;
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
        parse_target(&params.url).map_err(invalid_args)?;

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
        match tokio::time::timeout(budget, self.fetch(&params, &ctx)).await {
            Ok(result) => result.map_err(failed),
            Err(_elapsed) => Err(ToolError::Timeout {
                tool: ID.to_owned(),
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
pub fn parse_target(url: &str) -> Result<url::Url, WebError> {
    let parsed = url::Url::parse(url).map_err(|source| WebError::MalformedUrl {
        url: url.to_owned(),
        source,
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(WebError::UnsupportedScheme {
            url: url.to_owned(),
        });
    }
    Ok(parsed)
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
fn is_cloudflare_challenge(response: &reqwest::Response) -> bool {
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
fn classify_send_error(url: &str, source: reqwest::Error) -> WebError {
    if source.is_redirect() {
        return WebError::TooManyRedirects {
            limit: MAX_REDIRECTS,
        };
    }
    WebError::Transport {
        url: url.to_owned(),
        source,
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
        assert_eq!(parsed.scheme(), "http");
        assert_eq!(parsed.as_str(), "http://example.test/x");
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
}
