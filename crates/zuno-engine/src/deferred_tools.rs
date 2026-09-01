//! Turn-scoped progressive disclosure for connected tools.
//!
//! Executable tools remain registered at the dispatch boundary, but deferred
//! definitions are omitted from provider requests until the model searches their
//! metadata through [`TOOL_SEARCH_ID`]. This mirrors Skill progressive disclosure:
//! metadata search is cheap, while the full JSON schema is paid for only after a
//! capability is selected.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use zuno_error::ToolError;
use zuno_tool::{
    Tool, ToolConcurrencyPolicy, ToolContext, ToolDefinition, ToolEffect, ToolOutput,
    ToolReplayPolicy, TypedTool, erase,
};

/// The provider-visible discovery tool.
pub const TOOL_SEARCH_ID: &str = "tool_search";

const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 20;
const DESCRIPTION_PREVIEW_CHARS: usize = 240;

/// Provider-facing description of the discovery contract.
const DESCRIPTION: &str = "\
Search metadata for connected tools that are not currently in the provider tool list. \
Use a concise capability query before claiming an MCP or connected-service tool is \
unavailable. Matching tool definitions become callable on the next model step; repeat \
the search when a different capability is needed.";

#[derive(Debug, Default)]
struct Exposure {
    exposed: BTreeSet<String>,
    revision: u64,
}

/// Immutable candidate metadata plus the monotonically growing visible subset.
#[derive(Debug)]
pub(crate) struct DeferredToolCatalog {
    candidates: Vec<ToolDefinition>,
    candidate_ids: BTreeSet<String>,
    exposure: Mutex<Exposure>,
}

impl DeferredToolCatalog {
    pub(crate) fn new(candidates: Vec<ToolDefinition>) -> Option<Arc<Self>> {
        if candidates.is_empty() {
            return None;
        }
        let candidate_ids = candidates
            .iter()
            .map(|definition| definition.id.clone())
            .collect();
        Some(Arc::new(Self {
            candidates,
            candidate_ids,
            exposure: Mutex::new(Exposure::default()),
        }))
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.candidate_ids.contains(id)
    }

    pub(crate) fn is_exposed(&self, id: &str) -> bool {
        lock(&self.exposure).exposed.contains(id)
    }

    pub(crate) fn revision(&self) -> u64 {
        lock(&self.exposure).revision
    }

    pub(crate) fn search_tool(self: &Arc<Self>) -> Arc<dyn Tool> {
        erase(ToolSearch {
            catalog: Arc::clone(self),
        })
    }

    fn search(&self, query: &str, limit: usize) -> SearchOutcome {
        let normalized_query = normalize(query);
        let query_tokens = tokens(&normalized_query);
        let mut scored = self
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(index, definition)| {
                let score = score(definition, &normalized_query, &query_tokens);
                (score > 0).then_some((score, index, definition))
            })
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.id.cmp(&right.2.id))
        });
        let matches = scored
            .into_iter()
            .take(limit)
            .map(|(_, _, definition)| definition.clone())
            .collect::<Vec<_>>();

        let mut exposure = lock(&self.exposure);
        let mut newly_exposed = Vec::new();
        for definition in &matches {
            if exposure.exposed.insert(definition.id.clone()) {
                newly_exposed.push(definition.id.clone());
            }
        }
        if !newly_exposed.is_empty() {
            exposure.revision = exposure.revision.saturating_add(1);
        }
        let remaining = self.candidates.len().saturating_sub(exposure.exposed.len());
        SearchOutcome {
            matches,
            newly_exposed,
            revision: exposure.revision,
            remaining,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ToolSearchParams {
    /// Concise capability, service, or operation to find.
    query: String,
    /// Maximum matching definitions to expose.
    #[serde(default)]
    #[schemars(range(min = 1, max = 20))]
    limit: Option<usize>,
}

#[derive(Debug)]
struct ToolSearch {
    catalog: Arc<DeferredToolCatalog>,
}

#[async_trait]
impl TypedTool for ToolSearch {
    type Params = ToolSearchParams;

    fn id(&self) -> &str {
        TOOL_SEARCH_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::Exclusive
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn run(
        &self,
        params: ToolSearchParams,
        _ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let query = params.query.trim();
        if query.is_empty() {
            return Err(invalid("`query` must not be empty"));
        }
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(invalid(format!(
                "`limit` must be between 1 and {MAX_LIMIT}"
            )));
        }

        let outcome = self.catalog.search(query, limit);
        let matched_ids = outcome
            .matches
            .iter()
            .map(|definition| Value::String(definition.id.clone()))
            .collect::<Vec<_>>();
        let new_ids = outcome
            .newly_exposed
            .iter()
            .cloned()
            .map(Value::String)
            .collect::<Vec<_>>();
        let output = if outcome.matches.is_empty() {
            format!(
                "No deferred tools matched `{query}`. Try a broader capability, service, \
                 or operation name. {} deferred tool(s) remain searchable.",
                outcome.remaining
            )
        } else {
            let entries = outcome
                .matches
                .iter()
                .map(|definition| {
                    let preview = preview(&definition.description);
                    if preview.is_empty() {
                        format!("- `{}` ({})", definition.id, definition.display_name)
                    } else {
                        format!(
                            "- `{}` ({}) — {preview}",
                            definition.id, definition.display_name
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "Matched {} deferred tool(s); {} newly exposed for the next model step.\n\
                 {entries}\n\
                 Do not issue a newly exposed call in this same assistant message. \
                 Continue after this result, when the updated definitions are available.",
                outcome.matches.len(),
                outcome.newly_exposed.len(),
            )
        };

        Ok(ToolOutput::text(
            format!(
                "Tool search · {} match{}",
                outcome.matches.len(),
                if outcome.matches.len() == 1 { "" } else { "es" }
            ),
            output,
        )
        .with_metadata("query", query)
        .with_metadata("matchedTools", Value::Array(matched_ids))
        .with_metadata("newlyExposedTools", Value::Array(new_ids))
        .with_metadata("toolCatalogRevision", outcome.revision)
        .with_metadata(
            "remainingDeferredTools",
            u64::try_from(outcome.remaining).unwrap_or(u64::MAX),
        ))
    }
}

struct SearchOutcome {
    matches: Vec<ToolDefinition>,
    newly_exposed: Vec<String>,
    revision: u64,
    remaining: usize,
}

fn score(definition: &ToolDefinition, query: &str, query_tokens: &[String]) -> u32 {
    let id = normalize(&definition.id);
    let display = normalize(&definition.display_name);
    let description = normalize(&definition.description);
    let id_tokens = tokens(&id);
    let mut score = 0_u32;

    if id == query || display == query {
        score = score.saturating_add(1_000);
    } else {
        if id.starts_with(query) || display.starts_with(query) {
            score = score.saturating_add(500);
        }
        if id.contains(query) || display.contains(query) {
            score = score.saturating_add(250);
        }
    }

    let mut matched_all = !query_tokens.is_empty();
    for token in query_tokens {
        let token_score = if id_tokens.iter().any(|candidate| candidate == token) {
            140
        } else if id.contains(token) {
            100
        } else if display.contains(token) {
            70
        } else if description.contains(token) {
            35
        } else {
            matched_all = false;
            0
        };
        score = score.saturating_add(token_score);
    }
    if matched_all {
        score = score.saturating_add(100);
    }
    score
}

fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

fn tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| {
            !character.is_alphanumeric() && !matches!(character, '_' | '-' | '/')
        })
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
}

fn preview(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= DESCRIPTION_PREVIEW_CHARS {
        return normalized;
    }
    let mut output = normalized
        .chars()
        .take(DESCRIPTION_PREVIEW_CHARS.saturating_sub(3))
        .collect::<String>();
    output.push_str("...");
    output
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: TOOL_SEARCH_ID.to_owned(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message.into(),
        )),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn definition(id: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: description.to_owned(),
            parameters: json!({"type": "object", "properties": {}}),
            ui_intent: zuno_tool::ToolUiIntent::Generic,
        }
    }

    #[test]
    fn exact_names_rank_before_description_only_matches() {
        let catalog = DeferredToolCatalog::new(vec![
            definition("browser_open", "Open a page in Chrome."),
            definition("network_read", "Inspect browser requests."),
        ])
        .expect("catalog");

        let outcome = catalog.search("browser_open", 8);

        assert_eq!(outcome.matches[0].id, "browser_open");
        assert_eq!(outcome.newly_exposed, ["browser_open"]);
        assert_eq!(outcome.revision, 1);
    }

    #[test]
    fn repeated_searches_expand_monotonically() {
        let catalog = DeferredToolCatalog::new(vec![
            definition("browser_open", "Open a page."),
            definition("network_read", "Inspect network traffic."),
        ])
        .expect("catalog");

        let first = catalog.search("page", 8);
        let repeat = catalog.search("page", 8);
        let second = catalog.search("network", 8);

        assert_eq!(first.newly_exposed, ["browser_open"]);
        assert!(repeat.newly_exposed.is_empty());
        assert_eq!(repeat.revision, 1);
        assert_eq!(second.newly_exposed, ["network_read"]);
        assert_eq!(second.revision, 2);
        assert!(catalog.is_exposed("browser_open"));
        assert!(catalog.is_exposed("network_read"));
    }
}
