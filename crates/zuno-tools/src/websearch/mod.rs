//! Native Zuno web-search capability and model-facing batch consumer.

pub mod gating;
pub mod mcp;
mod provider;

use async_trait::async_trait;
use gating::SearchConfig;
use provider::McpSearchProvider;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use zuno_error::{BoxSource, ToolError};
use zuno_tool::{
    InterruptHandle, PermissionAsk, ToolConcurrencyPolicy, ToolContext, ToolEffect, ToolOutput,
    ToolReplayPolicy, TypedTool,
};

pub use provider::{
    SearchExecution, SearchRequest, SearchResult, SearchSource, WebSearchProvider,
    WebSearchProviderError,
};

/// Native wire id and permission key.
pub const ID: &str = "web_search";

/// Message returned when every provider result is empty.
pub const NO_RESULTS: &str = "No search results found. Please try different queries.";

/// Default number of distinct queries accepted in one call.
pub const DEFAULT_MAX_QUERIES: usize = 4;

/// Default combined source count returned to the model.
pub const DEFAULT_MAX_RESULTS: usize = 8;

/// Batch policy controlled by the runtime profile, never by model arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSearchPolicy {
    /// Maximum submitted queries before duplicate removal.
    pub max_queries: usize,
    /// Maximum sources after round-robin merge and URL deduplication.
    pub max_results: usize,
    /// Time budget for each provider request.
    pub timeout: Duration,
}

impl Default for WebSearchPolicy {
    fn default() -> Self {
        Self {
            max_queries: DEFAULT_MAX_QUERIES,
            max_results: DEFAULT_MAX_RESULTS,
            timeout: mcp::TIMEOUT,
        }
    }
}

/// Model-facing arguments. Runtime bounds and provider controls are profile config.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearchParams {
    /// One or more non-empty search queries.
    pub queries: Vec<String>,
}

/// Concurrent batch consumer over a single-query provider.
#[derive(Clone)]
pub struct WebSearchTool {
    provider: Arc<dyn WebSearchProvider>,
    config: SearchConfig,
    policy: WebSearchPolicy,
    description: String,
    mcp_backed: bool,
}

impl WebSearchTool {
    /// Build the configured hosted-provider adapter.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(SearchConfig::from_env())
    }

    /// Build the hosted-provider adapter from explicit configuration.
    #[must_use]
    pub fn with_config(config: SearchConfig) -> Self {
        let provider = Arc::new(McpSearchProvider::new(config.clone()));
        let policy = WebSearchPolicy {
            max_queries: config.max_queries,
            max_results: config.max_results,
            timeout: config.timeout,
        };
        Self {
            provider,
            config,
            policy,
            description: describe(policy.max_queries),
            mcp_backed: true,
        }
    }

    /// Build the consumer over a runtime-provided search backend.
    #[must_use]
    pub fn with_provider(provider: Arc<dyn WebSearchProvider>, policy: WebSearchPolicy) -> Self {
        Self {
            provider,
            config: SearchConfig::default(),
            policy,
            description: describe(policy.max_queries),
            mcp_backed: false,
        }
    }

    /// Point the hosted adapter at a test endpoint.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        assert!(
            self.mcp_backed,
            "with_endpoint is only valid for the hosted MCP adapter"
        );
        self.provider =
            Arc::new(McpSearchProvider::new(self.config.clone()).with_endpoint(endpoint));
        self
    }

    /// Override the per-query budget.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.policy.timeout = timeout;
        self
    }

    /// Provider and exposure configuration.
    #[must_use]
    pub const fn config(&self) -> &SearchConfig {
        &self.config
    }

    /// Whether this consumer has a usable provider.
    #[must_use]
    pub fn enabled_for(&self, _model_provider_id: &str) -> bool {
        !self.mcp_backed || gating::web_search_usable(&self.config)
    }

    /// Hosted backend selected for one session.
    #[must_use]
    pub fn provider_for(&self, session_id: &str) -> gating::Provider {
        gating::select_provider(session_id, &self.config)
    }

    async fn search_queries(
        &self,
        queries: &[String],
        ctx: &ToolContext,
    ) -> Result<SearchResult, ToolError> {
        if queries.len() == 1 {
            return self
                .search_one(&queries[0], Arc::clone(&ctx.interrupt), &ctx.session_id)
                .await;
        }

        let batch = Arc::new(BatchInterrupt::new(Arc::clone(&ctx.interrupt)));
        let mut tasks = JoinSet::new();
        for (index, query) in queries.iter().cloned().enumerate() {
            let provider = Arc::clone(&self.provider);
            let interrupt = Arc::clone(&batch);
            let session_id = ctx.session_id.clone();
            let timeout = self.policy.timeout;
            let max_results = self.policy.max_results;
            tasks.spawn(async move {
                let execution = SearchExecution {
                    session_id,
                    interrupt,
                };
                let result = tokio::time::timeout(
                    timeout,
                    provider.search(SearchRequest { query, max_results }, execution),
                )
                .await;
                (index, result)
            });
        }

        let mut results = vec![None; queries.len()];
        let mut first_failure = None;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((index, Ok(Ok(result)))) => results[index] = Some(result),
                Ok((_index, Ok(Err(source)))) => {
                    if first_failure.is_none() {
                        first_failure = Some(QueryFailure::Provider(source));
                        batch.cancel();
                    }
                }
                Ok((_index, Err(_elapsed))) => {
                    if first_failure.is_none() {
                        first_failure = Some(QueryFailure::Timeout(self.policy.timeout));
                        batch.cancel();
                    }
                }
                Err(source) => {
                    if first_failure.is_none() {
                        first_failure = Some(QueryFailure::Join(Box::new(source)));
                        batch.cancel();
                    }
                }
            }
        }
        if let Some(failure) = first_failure {
            return Err(failure.into_tool_error());
        }
        let results = results
            .into_iter()
            .map(|result| result.expect("successful query stored its result"))
            .collect::<Vec<_>>();
        Ok(merge_results(queries, &results, self.policy.max_results))
    }

    async fn search_one(
        &self,
        query: &str,
        interrupt: Arc<dyn InterruptHandle>,
        session_id: &str,
    ) -> Result<SearchResult, ToolError> {
        match tokio::time::timeout(
            self.policy.timeout,
            self.provider.search(
                SearchRequest {
                    query: query.to_owned(),
                    max_results: self.policy.max_results,
                },
                SearchExecution {
                    session_id: session_id.to_owned(),
                    interrupt,
                },
            ),
        )
        .await
        {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(source)) => Err(provider_error(source)),
            Err(_elapsed) => Err(ToolError::Timeout {
                tool: ID.to_owned(),
                elapsed: self.policy.timeout,
            }),
        }
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

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::ParallelSafe
    }

    fn effect(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn run(
        &self,
        params: WebSearchParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let queries = validate_queries(params.queries, self.policy.max_queries)?;
        ctx.ask(
            ID,
            PermissionAsk {
                permission: ID.to_owned(),
                patterns: queries.clone(),
                metadata: json!({
                    "queries": queries,
                    "provider": self.provider.id(),
                })
                .as_object()
                .expect("object")
                .clone(),
                always: vec!["*".to_owned()],
                ..PermissionAsk::default()
            },
        )
        .await?;

        let result = self.search_queries(&queries, &ctx).await?;
        let title = format!("Web search: {}", queries.join(", "));
        let mut output = ToolOutput::text(title, format_output(&result))
            .with_metadata("provider", self.provider.id())
            .with_metadata("queries", json!(queries))
            .with_metadata("truncated", result.truncated);
        output
            .metadata
            .insert("sources".to_owned(), json!(result.sources));
        Ok(output)
    }
}

fn validate_queries(queries: Vec<String>, max_queries: usize) -> Result<Vec<String>, ToolError> {
    if queries.is_empty() {
        return Err(invalid_args("queries must contain at least one query"));
    }
    if queries.len() > max_queries {
        let noun = if max_queries == 1 { "query" } else { "queries" };
        return Err(invalid_args(format!(
            "queries must contain at most {max_queries} {noun}"
        )));
    }
    if queries.iter().any(|query| query.trim().is_empty()) {
        return Err(invalid_args("each query must be a non-empty string"));
    }
    let mut seen = HashSet::new();
    Ok(queries
        .into_iter()
        .filter(|query| seen.insert(query.clone()))
        .collect())
}

fn invalid_args(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: ID.to_owned(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message.into(),
        )),
    }
}

fn merge_results(queries: &[String], results: &[SearchResult], max_results: usize) -> SearchResult {
    let max_rank = results
        .iter()
        .map(|result| result.sources.len())
        .max()
        .unwrap_or(0);
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    let mut dropped = false;
    'rank: for rank in 0..max_rank {
        for result in results {
            let Some(source) = result.sources.get(rank) else {
                continue;
            };
            if !seen.insert(source.url.clone()) {
                continue;
            }
            if sources.len() == max_results {
                dropped = true;
                break 'rank;
            }
            sources.push(source.clone());
        }
    }
    let content = results
        .iter()
        .enumerate()
        .filter_map(|(index, result)| {
            let content = result.content.as_deref()?.trim();
            (!content.is_empty()).then(|| format!("### {}\n\n{content}", queries[index]))
        })
        .collect::<Vec<_>>();
    SearchResult {
        content: (!content.is_empty()).then(|| content.join("\n\n")),
        sources,
        truncated: dropped || results.iter().any(|result| result.truncated),
    }
}

fn format_output(result: &SearchResult) -> String {
    let mut sections = Vec::new();
    if let Some(content) = result
        .content
        .as_ref()
        .filter(|content| !content.is_empty())
    {
        sections.push(content.clone());
    }
    if !result.sources.is_empty() {
        let sources = result
            .sources
            .iter()
            .map(|source| {
                let label = source.title.as_deref().unwrap_or(&source.url);
                let mut metadata = Vec::new();
                if let Some(snippet) = source.snippet.as_ref().filter(|value| !value.is_empty()) {
                    metadata.push(snippet.clone());
                }
                if let Some(date) = source
                    .published_at
                    .as_ref()
                    .filter(|value| !value.is_empty())
                {
                    metadata.push(format!("({date})"));
                }
                let suffix = if metadata.is_empty() {
                    String::new()
                } else {
                    format!(" - {}", metadata.join(" "))
                };
                format!("- [{label}]({}){suffix}", source.url)
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("Sources:\n{sources}"));
    }
    if sections.is_empty() {
        sections.push(NO_RESULTS.to_owned());
    }
    if result.truncated {
        sections.push(format!(
            "(Showing the first {} sources. Refine the queries for more.)",
            result.sources.len()
        ));
    }
    sections.push("Cite the relevant URLs above as markdown links in your answer.".to_owned());
    sections.join("\n\n")
}

/// Description names the configured query bound without exposing other controls.
#[must_use]
pub fn describe(max_queries: usize) -> String {
    format!(
        "Search the web for current information. Provide 1-{max_queries} non-empty queries in the required queries array. Distinct queries run concurrently and their sources are merged."
    )
}

enum QueryFailure {
    Provider(WebSearchProviderError),
    Timeout(Duration),
    Join(BoxSource),
}

impl QueryFailure {
    fn into_tool_error(self) -> ToolError {
        match self {
            Self::Provider(source) => provider_error(source),
            Self::Join(source) => ToolError::Failed {
                tool: ID.to_owned(),
                source,
            },
            Self::Timeout(elapsed) => ToolError::Timeout {
                tool: ID.to_owned(),
                elapsed,
            },
        }
    }
}

fn provider_error(error: WebSearchProviderError) -> ToolError {
    match error {
        WebSearchProviderError::Transient {
            retry_after,
            source,
        } => ToolError::Transient {
            tool: ID.to_owned(),
            retry_after,
            source,
        },
        WebSearchProviderError::Failed { source } => ToolError::Failed {
            tool: ID.to_owned(),
            source,
        },
    }
}

struct BatchInterrupt {
    parent: Arc<dyn InterruptHandle>,
    cancelled: AtomicBool,
    notify: Notify,
}

impl BatchInterrupt {
    fn new(parent: Arc<dyn InterruptHandle>) -> Self {
        Self {
            parent,
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            self.notify.notify_waiters();
        }
    }
}

#[async_trait]
impl InterruptHandle for BatchInterrupt {
    fn is_set(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.parent.is_set()
    }

    async fn notified(&self) {
        if self.is_set() {
            return;
        }
        let local = self.notify.notified();
        tokio::pin!(local);
        local.as_mut().enable();
        if self.is_set() {
            return;
        }
        tokio::select! {
            () = self.parent.notified() => {}
            () = &mut local => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_deduplicates_exact_queries_after_checking_the_bound() {
        assert_eq!(
            validate_queries(vec!["one".into(), "one".into(), " two ".into()], 4).expect("valid"),
            vec!["one", " two "]
        );
        assert!(validate_queries(vec!["one".into(), "one".into()], 1).is_err());
    }
}
