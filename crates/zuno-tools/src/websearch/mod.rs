//! `websearch`: query the session's search backend, when one is configured.
//!
//! # Gating, not failing
//!
//! Whether this tool is offered at all is [`gating::web_search_enabled`], evaluated
//! before the tool list is built. An unconfigured `websearch` is **absent** from the
//! list rather than present and failing — see the [`gating`] module docs for why a
//! tool the model cannot use is worse than no tool.
//!
//! # Registry key versus wire id
//!
//! Upstream registers this under the key `search`
//! (`packages/opencode/src/tool/registry.ts:218`, `search: Tool.init(websearch)`) while
//! the id the model calls is `websearch` (`Tool.define("websearch", …)`). As with
//! `webfetch`/`fetch`, [`Tool::id`](zuno_tool::Tool::id) is the wire id, which is also
//! the permission key.
//!
//! # Results are data
//!
//! Search results are pages a stranger wrote. They land in
//! [`ToolOutput::output`] as text with no structural privilege, exactly as
//! `webfetch`'s do.

pub mod gating;
pub mod mcp;

use async_trait::async_trait;
use gating::{Provider, SearchConfig};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use zuno_error::ToolError;
use zuno_tool::{ToolContext, ToolOutput, TypedTool};

/// The wire id, and the permission key.
pub const ID: &str = "websearch";

/// The message returned when the backend found nothing.
///
/// Oracle: `packages/core/src/tool/websearch.ts:20` (`NO_RESULTS`).
pub const NO_RESULTS: &str = "No search results found. Please try a different query.";

/// The default number of results requested.
///
/// Oracle: `params.numResults || 8` (`packages/core/src/tool/websearch.ts:221`).
pub const DEFAULT_NUM_RESULTS: u32 = 8;

/// The `{{year}}` placeholder the description carries.
const YEAR_PLACEHOLDER: &str = "{{year}}";

/// The description template, verbatim from
/// `packages/opencode/src/tool/websearch.txt`.
///
/// `{{year}}` is substituted at construction, because upstream does the same
/// (`websearch.ts:106-108`, `DESCRIPTION.replace("{{year}}", …)`) and the instruction
/// only works if the year is the current one.
pub const DESCRIPTION_TEMPLATE: &str = "- Search the web using the session's web search provider - performs real-time web searches and can scrape content from specific URLs
- Provides up-to-date information for current events and recent data
- Supports configurable result counts and returns the content from the most relevant websites
- Use this tool for accessing information beyond knowledge cutoff
- Searches are performed automatically within a single API call

Usage notes:
  - Supports live crawling modes when available: 'fallback' (backup if cached unavailable) or 'preferred' (prioritize live crawling)
  - Search types when available: 'auto' (balanced), 'fast' (quick results), 'deep' (comprehensive search)
  - Configurable context length for optimal LLM integration
  - Domain filtering and advanced search options available

The current year is {{year}}. You MUST use this year when searching for recent information or current events
- Example: If the current year is 2026 and the user asks for \"latest AI news\", search for \"AI news 2026\", NOT \"AI news 2025\"";

/// How aggressively the backend should crawl rather than serve cached content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LiveCrawl {
    /// Use live crawling as backup if cached content unavailable.
    Fallback,
    /// Prioritize live crawling.
    Preferred,
}

impl LiveCrawl {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::Preferred => "preferred",
        }
    }
}

/// How much work the backend should spend on the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchType {
    /// Balanced search.
    Auto,
    /// Quick results.
    Fast,
    /// Comprehensive search.
    Deep,
}

impl SearchType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fast => "fast",
            Self::Deep => "deep",
        }
    }
}

/// The arguments, deserialized and schema-derived from one declaration.
///
/// The wire names are camelCase because the model sees them: upstream's parameters
/// are `numResults` and `contextMaxCharacters`
/// (`packages/core/src/tool/websearch.ts:40-57`), and renaming them here would break
/// every model tuned against upstream's schema. `rename_all` drives both the derived
/// schema and the deserializer, so the two cannot drift.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebSearchParams {
    /// Websearch query
    pub query: String,
    /// Number of search results to return (default: 8)
    #[serde(default)]
    pub num_results: Option<u32>,
    /// Live crawl mode - 'fallback': use live crawling as backup if cached content unavailable, 'preferred': prioritize live crawling (default: 'fallback')
    #[serde(default)]
    pub livecrawl: Option<LiveCrawl>,
    /// Search type - 'auto': balanced search (default), 'fast': quick results, 'deep': comprehensive search
    #[serde(default)]
    pub r#type: Option<SearchType>,
    /// Maximum characters for context string optimized for LLMs (default: 10000)
    #[serde(default)]
    pub context_max_characters: Option<u32>,
}

/// Searches the web through the session's configured backend.
pub struct WebSearchTool {
    client: reqwest::Client,
    config: SearchConfig,
    description: String,
    /// The endpoint override tests point at a `wiremock` server.
    endpoint_override: Option<String>,
    /// The time budget, defaulting to [`mcp::TIMEOUT`].
    timeout: Duration,
}

impl WebSearchTool {
    /// A tool reading its configuration from the environment.
    ///
    /// # Panics
    /// Never in practice; see [`crate::webfetch::WebFetchTool::new`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(SearchConfig::from_env())
    }

    /// A tool over an explicit configuration.
    #[must_use]
    pub fn with_config(config: SearchConfig) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(
                    crate::webfetch::bounds::MAX_REDIRECTS,
                ))
                .build()
                .expect("reqwest client with a redirect policy"),
            config,
            description: describe(current_year()),
            endpoint_override: None,
            timeout: mcp::TIMEOUT,
        }
    }

    /// Points every call at `url` instead of the provider's real endpoint.
    ///
    /// Exists so the transport can be tested against `wiremock`; no test in this
    /// workspace may reach a real search backend.
    #[must_use]
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint_override = Some(url.into());
        self
    }

    /// Shortens the time budget, so a test can prove the call is bounded without
    /// waiting out upstream's real 25 seconds on every run.
    ///
    /// The default is pinned separately, so shortening it here cannot hide a wrong
    /// default.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The configuration in force.
    #[must_use]
    pub const fn config(&self) -> &SearchConfig {
        &self.config
    }

    /// Whether this tool should be offered for a turn served by `provider_id`.
    ///
    /// The predicate todo 44's registry filters on; see
    /// [`gating::web_search_enabled`].
    #[must_use]
    pub fn enabled_for(&self, provider_id: &str) -> bool {
        gating::web_search_enabled(provider_id, &self.config)
    }

    /// The backend this session routes to.
    #[must_use]
    pub fn provider_for(&self, session_id: &str) -> Provider {
        gating::select_provider(session_id, &self.config)
    }

    fn endpoint(&self, provider: Provider) -> String {
        self.endpoint_override
            .clone()
            .unwrap_or_else(|| mcp::endpoint(provider, self.config.api_key(provider)))
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TypedTool for WebSearchTool {
    type Params = WebSearchParams;

    fn id(&self) -> &str {
        ID
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn run(
        &self,
        params: WebSearchParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        if params.query.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: ID.to_owned(),
                source: "query must not be empty".into(),
            });
        }

        let provider = self.provider_for(&ctx.session_id);

        ctx.ask(
            ID,
            zuno_tool::PermissionAsk {
                permission: ID.to_owned(),
                patterns: vec![params.query.clone()],
                metadata: metadata(&params, provider),
                always: vec!["*".to_owned()],
            },
        )
        .await?;

        let url = self.endpoint(provider);
        let (tool, arguments) = match provider {
            Provider::Exa => (mcp::EXA_TOOL, exa_arguments(&params)),
            Provider::Parallel => (
                mcp::PARALLEL_TOOL,
                parallel_arguments(&params, &ctx.session_id),
            ),
        };
        let headers = auth_headers(provider, self.config.api_key(provider));

        let call = mcp::call(
            &self.client,
            &url,
            provider,
            tool,
            arguments,
            &headers,
            ctx.interrupt.as_ref(),
        );

        let text = match tokio::time::timeout(self.timeout, call).await {
            Ok(Ok(text)) => text,
            Ok(Err(error)) => {
                return Err(ToolError::Failed {
                    tool: ID.to_owned(),
                    source: Box::new(error),
                });
            }
            Err(_elapsed) => {
                return Err(ToolError::Timeout {
                    tool: ID.to_owned(),
                    elapsed: self.timeout,
                });
            }
        };

        let output = if text.trim().is_empty() {
            NO_RESULTS.to_owned()
        } else {
            text
        };

        Ok(
            ToolOutput::text(format!("{}: {}", provider.label(), params.query), output)
                .with_metadata("provider", provider.as_str()),
        )
    }
}

/// Exa's argument shape, with upstream's defaults applied.
///
/// Oracle: `packages/core/src/tool/websearch.ts:218-224`.
fn exa_arguments(params: &WebSearchParams) -> Value {
    let mut arguments = json!({
        "query": params.query,
        "type": params.r#type.unwrap_or(SearchType::Auto).as_str(),
        "numResults": params.num_results.unwrap_or(DEFAULT_NUM_RESULTS),
        "livecrawl": params.livecrawl.unwrap_or(LiveCrawl::Fallback).as_str(),
    });
    if let Some(max) = params.context_max_characters {
        arguments["contextMaxCharacters"] = json!(max);
    }
    arguments
}

/// Parallel's argument shape.
///
/// Oracle: `packages/core/src/tool/websearch.ts:226-236` — Parallel takes an
/// objective plus a query list and ignores the Exa-shaped knobs, so passing them
/// would be inventing an API.
fn parallel_arguments(params: &WebSearchParams, session_id: &str) -> Value {
    json!({
        "objective": params.query,
        "search_queries": [params.query],
        "session_id": session_id,
    })
}

/// The auth headers for `provider`.
///
/// Oracle: `packages/opencode/src/tool/websearch.ts:57-62` — Parallel takes a bearer
/// token and a `User-Agent`; Exa's key is in the URL instead, so it contributes no
/// header. A missing Parallel key sends the `User-Agent` alone rather than an empty
/// `Authorization`, matching upstream's early return.
fn auth_headers(
    provider: Provider,
    api_key: Option<&str>,
) -> Vec<(reqwest::header::HeaderName, String)> {
    if provider != Provider::Parallel {
        return Vec::new();
    }

    let mut headers = vec![(reqwest::header::USER_AGENT, user_agent())];
    if let Some(key) = api_key {
        headers.push((reqwest::header::AUTHORIZATION, format!("Bearer {key}")));
    }
    headers
}

/// The `User-Agent` Parallel is sent.
///
/// Oracle: `opencode/${InstallationVersion}`. The version is this crate's, which is
/// the workspace version.
fn user_agent() -> String {
    format!("opencode/{}", env!("CARGO_PKG_VERSION"))
}

fn metadata(
    params: &WebSearchParams,
    provider: Provider,
) -> serde_json::Map<String, serde_json::Value> {
    let mut metadata = serde_json::Map::new();
    metadata.insert("query".to_owned(), json!(params.query));
    metadata.insert("numResults".to_owned(), json!(params.num_results));
    metadata.insert(
        "livecrawl".to_owned(),
        json!(params.livecrawl.map(LiveCrawl::as_str)),
    );
    metadata.insert(
        "type".to_owned(),
        json!(params.r#type.map(SearchType::as_str)),
    );
    metadata.insert(
        "contextMaxCharacters".to_owned(),
        json!(params.context_max_characters),
    );
    metadata.insert("provider".to_owned(), json!(provider.as_str()));
    metadata
}

/// Substitutes the year into [`DESCRIPTION_TEMPLATE`].
#[must_use]
pub fn describe(year: i32) -> String {
    DESCRIPTION_TEMPLATE.replace(YEAR_PLACEHOLDER, &year.to_string())
}

/// The current year, local where the offset is knowable and UTC otherwise.
///
/// Upstream reads `new Date().getFullYear()`, which is local. A container with no
/// timezone database cannot resolve a local offset, and being off by a year boundary
/// for a few hours is a better failure than refusing to describe the tool.
fn current_year() -> i32 {
    time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        .year()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_id_is_websearch_not_the_registry_key_search() {
        assert_eq!(WebSearchTool::new().id(), "websearch");
    }

    #[test]
    fn the_year_is_substituted_and_the_placeholder_is_gone() {
        let described = describe(2026);
        assert!(
            described.contains("The current year is 2026."),
            "{described}"
        );
        assert!(!described.contains(YEAR_PLACEHOLDER));
    }

    #[test]
    fn the_live_description_carries_a_plausible_year() {
        let tool = WebSearchTool::with_config(SearchConfig::default());
        assert!(!tool.description().contains(YEAR_PLACEHOLDER));
        assert!(tool.description().contains("The current year is 20"));
    }

    #[test]
    fn the_schema_is_derived_and_only_query_is_required() {
        let schema = zuno_tool::Tool::raw_parameters_schema(&zuno_tool::Typed(
            WebSearchTool::with_config(SearchConfig::default()),
        ));
        assert_eq!(schema["required"], json!(["query"]));
    }

    #[test]
    fn exa_arguments_carry_upstreams_defaults() {
        let params: WebSearchParams =
            serde_json::from_value(json!({ "query": "rust" })).expect("params");
        let arguments = exa_arguments(&params);
        assert_eq!(arguments["query"], "rust");
        assert_eq!(arguments["type"], "auto");
        assert_eq!(arguments["numResults"], 8);
        assert_eq!(arguments["livecrawl"], "fallback");
        assert!(
            arguments.get("contextMaxCharacters").is_none(),
            "an absent limit must not be sent as null: {arguments}"
        );
    }

    #[test]
    fn exa_arguments_pass_explicit_values_through() {
        let params: WebSearchParams = serde_json::from_value(json!({
            "query": "rust",
            "numResults": 3,
            "livecrawl": "preferred",
            "type": "deep",
            "contextMaxCharacters": 500,
        }))
        .expect("params");
        let arguments = exa_arguments(&params);
        assert_eq!(arguments["numResults"], 3);
        assert_eq!(arguments["livecrawl"], "preferred");
        assert_eq!(arguments["type"], "deep");
        assert_eq!(arguments["contextMaxCharacters"], 500);
    }

    #[test]
    fn parallel_arguments_use_the_objective_shape() {
        let params: WebSearchParams =
            serde_json::from_value(json!({ "query": "rust", "numResults": 3 })).expect("params");
        let arguments = parallel_arguments(&params, "ses_1");
        assert_eq!(arguments["objective"], "rust");
        assert_eq!(arguments["search_queries"], json!(["rust"]));
        assert_eq!(arguments["session_id"], "ses_1");
        assert!(
            arguments.get("numResults").is_none(),
            "Exa's knobs must not be invented for Parallel: {arguments}"
        );
    }

    #[test]
    fn exa_sends_no_auth_header_because_its_key_is_in_the_url() {
        assert!(auth_headers(Provider::Exa, Some("exa-key")).is_empty());
    }

    #[test]
    fn parallel_sends_a_bearer_token_and_a_user_agent() {
        let headers = auth_headers(Provider::Parallel, Some("par-key"));
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].0, reqwest::header::USER_AGENT);
        assert!(headers[0].1.starts_with("opencode/"), "{:?}", headers[0].1);
        assert_eq!(headers[1].0, reqwest::header::AUTHORIZATION);
        assert_eq!(headers[1].1, "Bearer par-key");
    }

    #[test]
    fn parallel_without_a_key_sends_no_empty_authorization() {
        let headers = auth_headers(Provider::Parallel, None);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, reqwest::header::USER_AGENT);
    }

    #[test]
    fn the_endpoint_carries_the_exa_key_and_never_the_parallel_one() {
        let exa = WebSearchTool::with_config(SearchConfig {
            exa_api_key: Some("exa-key".to_owned()),
            parallel_api_key: Some("par-key".to_owned()),
            ..SearchConfig::default()
        });
        assert!(exa.endpoint(Provider::Exa).contains("exaApiKey=exa-key"));
        let parallel = exa.endpoint(Provider::Parallel);
        assert_eq!(parallel, mcp::PARALLEL_URL);
        assert!(!parallel.contains("par-key"));
    }

    #[test]
    fn the_wire_parameter_names_are_upstreams_camel_case() {
        let schema = zuno_tool::Tool::raw_parameters_schema(&zuno_tool::Typed(
            WebSearchTool::with_config(SearchConfig::default()),
        ));
        let properties = schema["properties"].as_object().expect("properties");
        for key in [
            "query",
            "numResults",
            "livecrawl",
            "type",
            "contextMaxCharacters",
        ] {
            assert!(properties.contains_key(key), "missing {key}: {schema}");
        }
        assert!(
            !properties.contains_key("num_results"),
            "snake_case would break every model tuned against upstream: {schema}"
        );
    }

    #[test]
    fn an_unknown_field_is_rejected_rather_than_silently_dropped() {
        let error = serde_json::from_value::<WebSearchParams>(json!({
            "query": "rust",
            "num_results": 3,
        }))
        .expect_err("snake_case num_results is not the wire field name");
        assert!(error.to_string().contains("num_results"), "{error}");
    }
}
