//! Bounded parallel composition with dependency-aware variable binding.

mod binding;

use crate::output_policy::OutputPolicy;
use crate::registry::{RegistryHandle, canonical_tool_name};
use async_trait::async_trait;
use binding::{BindingError, BindingPlan, expand};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use tokio::task::JoinSet;
use zuno_error::ToolError;
use zuno_error::source::describe;
use zuno_tool::{OutputLimits, ToolContext, ToolEffect, ToolOutput, ToolOutputStore, TypedTool};

/// Maximum declared and expanded sub-calls in one composition.
pub const MAX_SUBCALLS: usize = 10;

/// Total output bytes shared by returned sub-call results.
pub const TOTAL_OUTPUT_BYTES: usize = 50_000;

/// The model-facing description stays terse; parameter docs carry the protocol.
pub const DESCRIPTION: &str = include_str!("description/batch.txt");

/// Arguments for one composed execution.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecuteParams {
    /// Tool calls to run. Independent calls run in parallel; references create dependencies.
    #[schemars(length(min = 1, max = 10))]
    pub tool_calls: Vec<Subcall>,
}

/// One tool call. Tool-specific arguments stay inline beside these control fields.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct Subcall {
    /// Registered tool name.
    pub tool: String,
    /// Short human-readable reason for this sub-call.
    pub intent: String,
    /// Optional name for later `$ref` or `$each` expressions.
    #[serde(default)]
    pub bind: Option<String>,
    /// Inline arguments for the selected tool. Use `{"$ref":"name.output"}` for one
    /// value or `{"$each":"name.metadata.files[*]"}` for bounded fan-out.
    #[serde(flatten)]
    pub arguments: Map<String, Value>,
}

/// Registry-backed implementation of the `execute` tool.
#[derive(Clone)]
pub struct ExecuteTool {
    registry: RegistryHandle,
    output_store: ToolOutputStore,
}

impl ExecuteTool {
    #[must_use]
    pub(crate) fn new(registry: RegistryHandle, output_store: ToolOutputStore) -> Self {
        Self {
            registry,
            output_store,
        }
    }
}

#[async_trait]
impl TypedTool for ExecuteTool {
    type Params = ExecuteParams;

    fn id(&self) -> &str {
        "execute"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::Delegating
    }

    async fn run(&self, params: ExecuteParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        validate_calls(&params.tool_calls)?;
        let plan = BindingPlan::new(&params.tool_calls).map_err(input_error)?;
        let mut bindings = BTreeMap::new();
        let mut results = Vec::new();
        let mut expanded_count = 0usize;

        for level in plan.levels() {
            let mut work = Vec::new();
            for &index in level {
                let call = &params.tool_calls[index];
                let mut arguments = call.arguments.clone();
                arguments.insert(
                    zuno_tool::INTENT_KEY.to_owned(),
                    Value::String(call.intent.clone()),
                );
                match expand(&arguments, &bindings) {
                    Ok(expanded) => {
                        expanded_count = expanded_count.saturating_add(expanded.len());
                        if expanded_count > MAX_SUBCALLS {
                            return Err(input_error(ExecuteInputError::ExpandedLimit {
                                actual: expanded_count,
                            }));
                        }
                        let fanout = expanded.len() > 1;
                        work.extend(expanded.into_iter().enumerate().map(|(item, arguments)| {
                            Work {
                                call_index: index,
                                fanout_index: fanout.then_some(item + 1),
                                tool: canonical_tool_name(&call.tool).to_owned(),
                                bind: call.bind.clone(),
                                accept_large_output: zuno_tool::guard::accepts_large_output(
                                    &arguments,
                                ),
                                arguments,
                            }
                        }));
                    }
                    Err(error) => results.push(Invocation::failed(
                        index,
                        canonical_tool_name(&call.tool),
                        input_error(error),
                    )),
                }
            }

            let registry = self.registry.clone();
            let parent = ctx.clone();
            let mut tasks = JoinSet::new();
            for work in work {
                let registry = registry.clone();
                let subctx = parent.for_subcall(work.call_id());
                tasks.spawn(async move {
                    let result = registry
                        .execute(&work.tool, work.arguments.clone(), subctx)
                        .await;
                    Invocation::completed(work, result)
                });
            }
            let mut completed = Vec::with_capacity(tasks.len());
            while let Some(joined) = tasks.join_next().await {
                completed.push(joined.map_err(|source| ToolError::Failed {
                    tool: "execute".to_owned(),
                    source: Box::new(source),
                })?);
            }

            bind_successes(&params.tool_calls, &completed, &mut bindings);
            results.extend(completed);
        }

        Ok(render(results, &ctx.session_id, &self.output_store))
    }
}

#[derive(Debug, thiserror::Error)]
enum ExecuteInputError {
    #[error("at least one sub-call is required")]
    Empty,
    #[error("maximum {MAX_SUBCALLS} sub-calls allowed; received {actual}")]
    DeclaredLimit { actual: usize },
    #[error("expanded fan-out would run {actual} sub-calls; maximum is {MAX_SUBCALLS}")]
    ExpandedLimit { actual: usize },
    #[error("Cannot execute the `execute` tool recursively")]
    Recursive,
    #[error(transparent)]
    Binding(#[from] BindingError),
}

fn validate_calls(calls: &[Subcall]) -> Result<(), ToolError> {
    if calls.is_empty() {
        return Err(input_error(ExecuteInputError::Empty));
    }
    if calls.len() > MAX_SUBCALLS {
        return Err(input_error(ExecuteInputError::DeclaredLimit {
            actual: calls.len(),
        }));
    }
    if calls
        .iter()
        .any(|call| canonical_tool_name(&call.tool) == "execute")
    {
        return Err(input_error(ExecuteInputError::Recursive));
    }
    Ok(())
}

fn input_error(error: impl Into<ExecuteInputError>) -> ToolError {
    ToolError::InvalidArgs {
        tool: "execute".to_owned(),
        source: Box::new(error.into()),
    }
}

struct Work {
    call_index: usize,
    fanout_index: Option<usize>,
    tool: String,
    bind: Option<String>,
    accept_large_output: bool,
    arguments: Value,
}

impl Work {
    fn call_id(&self) -> String {
        match self.fanout_index {
            Some(item) => format!("execute-{}.{item}-{}", self.call_index + 1, self.tool),
            None => format!("execute-{}-{}", self.call_index + 1, self.tool),
        }
    }
}

struct Invocation {
    call_index: usize,
    fanout_index: Option<usize>,
    tool: String,
    bind: Option<String>,
    accept_large_output: bool,
    result: Result<ToolOutput, ToolError>,
}

impl Invocation {
    fn completed(work: Work, result: Result<ToolOutput, ToolError>) -> Self {
        Self {
            call_index: work.call_index,
            fanout_index: work.fanout_index,
            tool: work.tool,
            bind: work.bind,
            accept_large_output: work.accept_large_output,
            result,
        }
    }

    fn failed(call_index: usize, tool: &str, error: ToolError) -> Self {
        Self {
            call_index,
            fanout_index: None,
            tool: tool.to_owned(),
            bind: None,
            accept_large_output: false,
            result: Err(error),
        }
    }

    const fn order(&self) -> (usize, Option<usize>) {
        (self.call_index, self.fanout_index)
    }
}

fn bind_successes(
    calls: &[Subcall],
    completed: &[Invocation],
    bindings: &mut BTreeMap<String, ToolOutput>,
) {
    for &index in completed
        .iter()
        .map(|result| result.call_index)
        .collect::<std::collections::BTreeSet<_>>()
        .iter()
    {
        let Some(name) = calls[index].bind.as_ref() else {
            continue;
        };
        let outputs: Vec<&ToolOutput> = completed
            .iter()
            .filter(|result| result.call_index == index)
            .filter_map(|result| result.result.as_ref().ok())
            .collect();
        if outputs.len() == 1 {
            bindings.insert(name.clone(), outputs[0].clone());
        }
    }
}

/// Render every sub-call result, failures included, into one model-facing block.
///
/// A failed sub-call is rendered through [`describe`] rather than its own `Display`.
/// `{error}` prints only the outermost link, so a sub-call that failed inside an MCP
/// server or a plugin host arrived here classified as `tool X failed` with the reason
/// still hanging off `source()` — every diagnosis reached through composition was lost
/// at this line.
fn render(mut results: Vec<Invocation>, session_id: &str, store: &ToolOutputStore) -> ToolOutput {
    results.sort_by_key(Invocation::order);
    let budget = TOTAL_OUTPUT_BYTES / results.len().max(1);
    let policy = OutputPolicy::new(
        store.clone(),
        OutputLimits {
            max_lines: usize::MAX,
            max_bytes: budget,
        },
    );
    let mut lines = Vec::new();
    let mut succeeded = 0usize;
    let mut failed = 0usize;

    for result in results {
        let label = match result.fanout_index {
            Some(item) => format!("{}.{}", result.call_index + 1, item),
            None => (result.call_index + 1).to_string(),
        };
        lines.push(format!("--- [{label}] {} ---", result.tool));
        match result.result {
            Ok(_output) if result.bind.is_some() => {
                succeeded += 1;
                lines.push(format!(
                    "(bound as `{}`; output withheld)",
                    result.bind.as_deref().unwrap_or_default()
                ));
            }
            Ok(output) => {
                // A sub-call whose output was withheld for size still succeeded, and its
                // rendered block is the notice naming the artifact and the windowed read.
                // Only a store that could not be written arrives here as an error, and
                // that is the one case where this batch really lost the output.
                match policy.apply(&result.tool, session_id, output, result.accept_large_output) {
                    Ok(output) => {
                        succeeded += 1;
                        lines.push(output.output);
                    }
                    Err(error) => {
                        failed += 1;
                        lines.push(format!("Error: {}", describe(&error)));
                    }
                }
            }
            Err(error) => {
                failed += 1;
                lines.push(format!("Error: {}", describe(&error)));
            }
        }
        lines.push(String::new());
    }
    lines.push(format!("Completed: {succeeded} succeeded, {failed} failed"));
    ToolOutput::text("Parallel tool execution", lines.join("\n"))
}
