//! The JSON-RPC call both search backends answer, bounded like every other fetch.
//!
//! Both providers expose their search as an MCP `tools/call`, so one bounded POST
//! serves both. Oracle: `packages/core/src/tool/websearch.ts:152-186`.
//!
//! # Two response shapes, one parser
//!
//! The request advertises `Accept: application/json, text/event-stream`, so a server
//! may answer with either a plain JSON envelope or an SSE stream whose `data:` lines
//! each carry one. Upstream tries the whole body as JSON first and then walks the
//! `data:` lines (`:114-123`); a parser that handled only one shape would work
//! against one provider and fail against the other.

use crate::webfetch::body::read_bounded;
use crate::webfetch::bounds::WebError;
use crate::websearch::gating::Provider;
use serde_json::{Value, json};
use std::fmt;
use std::time::Duration;
use zuno_network::DiagnosticEndpoint;
use zuno_tool::InterruptHandle;

/// Exa's MCP endpoint.
///
/// Oracle: `packages/core/src/tool/websearch.ts:21`.
pub const EXA_URL: &str = "https://mcp.exa.ai/mcp";

/// Parallel's MCP endpoint.
///
/// Oracle: `packages/core/src/tool/websearch.ts:22`.
pub const PARALLEL_URL: &str = "https://search.parallel.ai/mcp";

/// The cap on a search response body.
///
/// Oracle: `MAX_RESPONSE_BYTES = 256 * 1024`
/// (`packages/core/src/tool/websearch.ts:25`). Two orders of magnitude below
/// `webfetch`'s cap because a search result set is text, not a document.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// The time budget for one search call.
///
/// Oracle: `Duration.seconds(25)` (`packages/core/src/tool/websearch.ts:181`).
pub const TIMEOUT: Duration = Duration::from_secs(25);

/// The tool name invoked on Exa's server.
pub const EXA_TOOL: &str = "web_search_exa";

/// The tool name invoked on Parallel's server.
pub const PARALLEL_TOOL: &str = "web_search";

/// A credential-bearing wire URL paired with its safe diagnostic identity.
#[derive(Clone)]
pub struct SearchEndpoint {
    wire: url::Url,
    diagnostic: DiagnosticEndpoint,
    provider: Provider,
}

impl SearchEndpoint {
    /// Construct the hosted endpoint for a provider.
    #[must_use]
    pub fn hosted(provider: Provider, api_key: Option<&str>) -> Self {
        let raw = match provider {
            Provider::Exa => EXA_URL,
            Provider::Parallel => PARALLEL_URL,
        };
        let mut wire = url::Url::parse(raw).expect("shipped search endpoint is an absolute URL");
        if provider == Provider::Exa
            && let Some(key) = api_key
        {
            wire.query_pairs_mut().append_pair("exaApiKey", key);
        }
        Self {
            diagnostic: DiagnosticEndpoint::from_url(&wire),
            wire,
            provider,
        }
    }

    /// Parse an endpoint override without ever retaining its query in diagnostics.
    pub fn override_for(provider: Provider, raw: &str) -> Result<Self, WebError> {
        let wire = url::Url::parse(raw).map_err(|_| WebError::InvalidSearchEndpoint {
            provider: provider.as_str(),
        })?;
        if !matches!(wire.scheme(), "http" | "https") || wire.host().is_none() {
            return Err(WebError::InvalidSearchEndpoint {
                provider: provider.as_str(),
            });
        }
        Ok(Self {
            diagnostic: DiagnosticEndpoint::from_url(&wire),
            wire,
            provider,
        })
    }

    fn wire(&self) -> &str {
        self.wire.as_str()
    }

    fn diagnostic(&self) -> &DiagnosticEndpoint {
        &self.diagnostic
    }
}

impl fmt::Debug for SearchEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchEndpoint")
            .field("provider", &self.provider.as_str())
            .field("endpoint", &self.diagnostic)
            .finish()
    }
}

/// The JSON-RPC envelope for one search.
///
/// Oracle: `packages/core/src/tool/websearch.ts:137-143` — `id` is the literal `1`,
/// because one request is sent per call and nothing is multiplexed.
#[must_use]
pub fn request_body(tool: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    })
}

/// POSTs a search request and returns the text the backend produced.
///
/// Bounded in time by [`TIMEOUT`] and in size by [`MAX_RESPONSE_BYTES`]; `interrupt`
/// is polled while the body streams.
///
/// # Errors
/// - [`WebError::SearchStatus`] on a non-2xx answer.
/// - [`WebError::TooLarge`] when the response exceeds [`MAX_RESPONSE_BYTES`].
/// - [`WebError::MalformedSearchResponse`] when no envelope in the body carries text.
/// - [`WebError::SearchTransport`] or [`WebError::Interrupted`] from the body read.
pub async fn call(
    client: &reqwest::Client,
    endpoint: &SearchEndpoint,
    provider: Provider,
    tool: &str,
    arguments: Value,
    headers: &[(reqwest::header::HeaderName, String)],
    interrupt: &dyn InterruptHandle,
) -> Result<String, WebError> {
    let mut request = client
        .post(endpoint.wire())
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(&request_body(tool, arguments));
    for (name, value) in headers {
        request = request.header(name, value);
    }

    let response = tokio::select! {
        () = interrupt.notified() => {
            return Err(WebError::Interrupted { read: 0 });
        }
        response = request.send() => {
            response.map_err(|source| WebError::SearchTransport {
                provider: provider.as_str(),
                endpoint: endpoint.diagnostic().clone(),
                source: source.without_url(),
            })?
        }
    };

    let status = response.status();
    if !status.is_success() {
        return Err(WebError::SearchStatus {
            provider: provider.as_str(),
            endpoint: endpoint.diagnostic().clone(),
            status: status.as_u16(),
            retry_after: crate::webfetch::bounds::retry_after(response.headers()),
        });
    }

    let body = match read_bounded(response, MAX_RESPONSE_BYTES, interrupt).await {
        Err(WebError::Transport { source, .. }) => {
            return Err(WebError::SearchTransport {
                provider: provider.as_str(),
                endpoint: endpoint.diagnostic().clone(),
                source,
            });
        }
        result => result?,
    };
    parse_response(&String::from_utf8_lossy(&body)).ok_or(WebError::MalformedSearchResponse {
        provider: provider.as_str(),
    })
}

/// Extracts the first non-empty `result.content[].text` from a JSON or SSE body.
///
/// Oracle: `packages/core/src/tool/websearch.ts:108-124`.
#[must_use]
pub fn parse_response(body: &str) -> Option<String> {
    if let Some(text) = parse_payload(body) {
        return Some(text);
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .find_map(parse_payload)
}

/// Reads one JSON-RPC envelope, ignoring anything that is not an object.
///
/// The `starts_with('{')` guard is the oracle's (`:110`): an SSE comment or a bare
/// event name is not a parse failure, it is simply not the payload.
fn parse_payload(payload: &str) -> Option<String> {
    let trimmed = payload.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    serde_json::from_str::<Value>(trimmed)
        .ok()?
        .get("result")?
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text")?.as_str())
        .find(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(text: &str) -> String {
        json!({ "result": { "content": [{ "type": "text", "text": text }] } }).to_string()
    }

    #[test]
    fn a_plain_json_envelope_parses() {
        assert_eq!(
            parse_response(&envelope("results here")).as_deref(),
            Some("results here")
        );
    }

    #[test]
    fn an_sse_stream_parses() {
        let body = format!("event: message\ndata: {}\n\n", envelope("from sse"));
        assert_eq!(parse_response(&body).as_deref(), Some("from sse"));
    }

    #[test]
    fn an_sse_stream_skips_envelopes_with_no_text() {
        let empty = json!({ "result": { "content": [{ "type": "text", "text": "" }] } });
        let body = format!("data: {empty}\ndata: {}\n", envelope("second"));
        assert_eq!(parse_response(&body).as_deref(), Some("second"));
    }

    #[test]
    fn a_non_json_body_yields_nothing_rather_than_panicking() {
        assert_eq!(parse_response("<html>an error page</html>"), None);
        assert_eq!(parse_response(""), None);
        assert_eq!(parse_response("data: not json\n"), None);
    }

    #[test]
    fn an_envelope_without_content_yields_nothing() {
        assert_eq!(parse_response(&json!({ "result": {} }).to_string()), None);
        assert_eq!(
            parse_response(&json!({ "error": { "code": -1 } }).to_string()),
            None
        );
    }

    #[test]
    fn the_exa_key_rides_on_the_wire_but_not_in_diagnostics() {
        let endpoint = SearchEndpoint::hosted(Provider::Exa, Some("key/with+chars"));
        assert!(endpoint.wire().starts_with(EXA_URL));
        assert!(
            endpoint.wire().contains("exaApiKey=key%2Fwith%2Bchars"),
            "{}",
            endpoint.wire()
        );
        let diagnostic = format!("{endpoint:?}");
        assert!(!diagnostic.contains("key/with"));
        assert!(!diagnostic.contains("exaApiKey"));
    }

    #[test]
    fn no_exa_key_leaves_the_url_bare() {
        assert_eq!(SearchEndpoint::hosted(Provider::Exa, None).wire(), EXA_URL);
    }

    #[test]
    fn parallel_never_puts_its_key_in_the_url() {
        assert_eq!(
            SearchEndpoint::hosted(Provider::Parallel, Some("secret")).wire(),
            PARALLEL_URL
        );
    }

    #[test]
    fn override_diagnostics_drop_secret_query_parameters() {
        let endpoint = SearchEndpoint::override_for(
            Provider::Exa,
            "https://example.test/mcp?exaApiKey=sentinel-secret",
        )
        .unwrap();
        let rendered = format!("{endpoint:?}");
        assert!(!rendered.contains("sentinel-secret"));
        assert!(!rendered.contains("exaApiKey"));
        assert!(rendered.contains("https://example.test/mcp"));
    }

    #[test]
    fn overrides_require_an_http_endpoint_with_a_host() {
        for endpoint in [
            "file:///tmp/search",
            "data:text/plain,no",
            "mailto:test@example.com",
        ] {
            assert!(matches!(
                SearchEndpoint::override_for(Provider::Exa, endpoint),
                Err(WebError::InvalidSearchEndpoint { provider: "exa" })
            ));
        }
    }

    #[test]
    fn the_request_envelope_matches_the_oracle_shape() {
        let body = request_body(EXA_TOOL, json!({ "query": "q" }));
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], EXA_TOOL);
        assert_eq!(body["params"]["arguments"]["query"], "q");
    }
}
