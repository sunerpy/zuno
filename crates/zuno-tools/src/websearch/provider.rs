//! Provider-facing types and the hosted MCP adapter for web search.

use super::gating::{Provider, SearchConfig, select_provider};
use super::mcp;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use zuno_error::BoxSource;
use zuno_tool::InterruptHandle;

/// One provider-neutral search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    /// The query sent to one provider call.
    pub query: String,
    /// Maximum sources requested from that provider.
    pub max_results: usize,
}

/// One citeable search source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSource {
    /// Absolute source URL.
    pub url: String,
    /// Provider-supplied title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Provider-supplied excerpt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Provider-supplied publication or crawl time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
}

/// Normalized outcome of one provider search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResult {
    /// Provider answer or search context.
    pub content: Option<String>,
    /// Citeable sources in provider rank order.
    pub sources: Vec<SearchSource>,
    /// Whether the provider dropped sources before returning.
    pub truncated: bool,
}

/// Narrow execution context visible to a search provider.
#[derive(Clone)]
pub struct SearchExecution {
    /// Session used for provider routing and correlation.
    pub session_id: String,
    /// Caller and batch cancellation combined into one signal.
    pub interrupt: Arc<dyn InterruptHandle>,
}

/// A backend that performs one normalized web search.
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    /// Stable provider identity for diagnostics and result metadata.
    fn id(&self) -> &str;

    /// Run one search and honor `execution.interrupt`.
    async fn search(
        &self,
        request: SearchRequest,
        execution: SearchExecution,
    ) -> Result<SearchResult, BoxSource>;
}

/// Exa/Parallel MCP adapter selected from native Zuno configuration.
pub(crate) struct McpSearchProvider {
    client: reqwest::Client,
    config: SearchConfig,
    endpoint_override: Option<String>,
}

impl McpSearchProvider {
    pub(crate) fn new(config: SearchConfig) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(
                    crate::webfetch::bounds::MAX_REDIRECTS,
                ))
                .build()
                .expect("reqwest client with a redirect policy"),
            config,
            endpoint_override: None,
        }
    }

    pub(crate) fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint_override = Some(endpoint.into());
        self
    }

    fn endpoint(&self, provider: Provider) -> String {
        self.endpoint_override
            .clone()
            .unwrap_or_else(|| mcp::endpoint(provider, self.config.api_key(provider)))
    }
}

#[async_trait]
impl WebSearchProvider for McpSearchProvider {
    fn id(&self) -> &str {
        "mcp"
    }

    async fn search(
        &self,
        request: SearchRequest,
        execution: SearchExecution,
    ) -> Result<SearchResult, BoxSource> {
        let provider = select_provider(&execution.session_id, &self.config);
        let (tool, arguments) = match provider {
            Provider::Exa => (
                mcp::EXA_TOOL,
                json!({
                    "query": request.query,
                    "type": "auto",
                    "numResults": request.max_results,
                    "livecrawl": "fallback",
                }),
            ),
            Provider::Parallel => (
                mcp::PARALLEL_TOOL,
                json!({
                    "objective": request.query,
                    "search_queries": [request.query],
                    "session_id": execution.session_id,
                }),
            ),
        };
        let headers = auth_headers(provider, self.config.api_key(provider));
        let text = mcp::call(
            &self.client,
            &self.endpoint(provider),
            provider,
            tool,
            arguments,
            &headers,
            execution.interrupt.as_ref(),
        )
        .await?;
        Ok(normalize_provider_text(&text))
    }
}

fn auth_headers(
    provider: Provider,
    api_key: Option<&str>,
) -> Vec<(reqwest::header::HeaderName, String)> {
    if provider != Provider::Parallel {
        return Vec::new();
    }
    let mut headers = vec![(
        reqwest::header::USER_AGENT,
        format!("zuno/{}", env!("CARGO_PKG_VERSION")),
    )];
    if let Some(key) = api_key {
        headers.push((reqwest::header::AUTHORIZATION, format!("Bearer {key}")));
    }
    headers
}

/// Preserve provider text while projecting every citeable HTTP(S) URL.
pub(crate) fn normalize_provider_text(text: &str) -> SearchResult {
    let content = text.trim();
    if content.is_empty() {
        return SearchResult::default();
    }
    SearchResult {
        content: Some(content.to_owned()),
        sources: extract_sources(content),
        truncated: false,
    }
}

fn extract_sources(text: &str) -> Vec<SearchSource> {
    let mut sources = Vec::new();
    let mut seen = HashSet::new();

    let mut remainder = text;
    while let Some(close_label) = remainder.find("](") {
        let before = &remainder[..close_label];
        let after = &remainder[close_label + 2..];
        let Some(close_url) = after.find(')') else {
            break;
        };
        let candidate = &after[..close_url];
        let title = before
            .rfind('[')
            .map(|open| before[open + 1..].trim())
            .filter(|title| !title.is_empty())
            .map(str::to_owned);
        push_source(candidate, title, &mut seen, &mut sources);
        remainder = &after[close_url + 1..];
    }

    for token in text.split_whitespace() {
        let candidate = token
            .trim_start_matches(['(', '[', '<', '"', '\''])
            .trim_end_matches([')', ']', '>', '"', '\'', ',', '.', ';', ':', '!', '?']);
        push_source(candidate, None, &mut seen, &mut sources);
    }
    sources
}

fn push_source(
    candidate: &str,
    title: Option<String>,
    seen: &mut HashSet<String>,
    sources: &mut Vec<SearchSource>,
) {
    let Ok(url) = url::Url::parse(candidate) else {
        return;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return;
    }
    let rendered = url.to_string();
    if seen.insert(rendered.clone()) {
        sources.push(SearchSource {
            url: rendered,
            title,
            snippet: None,
            published_at: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_text_preserves_content_and_projects_markdown_and_bare_urls() {
        let result =
            normalize_provider_text("Read [Alpha](https://a.test/path) and URL: https://b.test/x.");
        assert_eq!(
            result.sources,
            vec![
                SearchSource {
                    url: "https://a.test/path".to_owned(),
                    title: Some("Alpha".to_owned()),
                    snippet: None,
                    published_at: None,
                },
                SearchSource {
                    url: "https://b.test/x".to_owned(),
                    title: None,
                    snippet: None,
                    published_at: None,
                },
            ]
        );
    }
}
