use super::CompiledGraph;
use super::GRAPH_ENGINE_REVISION;
use super::engine::graph_engine_error;
use crate::WorkflowCallIdentity;
use crate::WorkflowEngine;
use crate::WorkflowError;
use crate::WorkflowHost;
use crate::WorkflowHostCall;
use crate::WorkflowHostCallKind;
use crate::WorkflowNode;
use crate::WorkflowStartRequest;
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_node_call(
    calls: &mut JoinSet<(usize, Result<JsonValue, WorkflowError>)>,
    host: Arc<dyn WorkflowHost>,
    args: &JsonValue,
    nodes: &[WorkflowNode],
    graph: &CompiledGraph,
    results: &[Option<JsonValue>],
    index: usize,
    cancellation: CancellationToken,
) -> Result<(), WorkflowError> {
    let node = nodes
        .get(index)
        .ok_or_else(|| graph_engine_error(format!("compiled node index {index} is missing")))?;
    let payload = node_payload(node, args, nodes, graph, results, index)?;
    let identity = WorkflowCallIdentity::new(node.id.clone(), &payload)?;
    calls.spawn(async move {
        let result = host
            .call(
                WorkflowHostCall {
                    kind: WorkflowHostCallKind::Agent,
                    payload,
                    identity: Some(identity),
                },
                cancellation,
            )
            .await;
        (index, result)
    });
    Ok(())
}

fn node_payload(
    node: &WorkflowNode,
    args: &JsonValue,
    nodes: &[WorkflowNode],
    graph: &CompiledGraph,
    results: &[Option<JsonValue>],
    index: usize,
) -> Result<JsonValue, WorkflowError> {
    let mut needs = JsonMap::new();
    for dependency in graph.dependencies(index).unwrap_or_default() {
        let dependency_node = nodes.get(*dependency).ok_or_else(|| {
            graph_engine_error(format!("compiled dependency index {dependency} is missing"))
        })?;
        let dependency_value = results
            .get(*dependency)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                graph_engine_error(format!(
                    "dependency `{}` completed without a result",
                    dependency_node.id
                ))
            })?;
        needs.insert(dependency_node.id.clone(), dependency_value.clone());
    }
    let input = node.input.clone().unwrap_or(JsonValue::Null);
    let output_schema = node.output_schema.clone().unwrap_or(JsonValue::Null);
    let prompt = render_node_prompt(node, args, &input, &needs, &output_schema)?;
    Ok(json!({
        "id": node.id,
        "route": node.route,
        "prompt": prompt,
        "input": input,
        "args": args,
        "needs": needs,
        "outputSchema": output_schema,
    }))
}

fn render_node_prompt(
    node: &WorkflowNode,
    args: &JsonValue,
    input: &JsonValue,
    needs: &JsonMap<String, JsonValue>,
    output_schema: &JsonValue,
) -> Result<String, WorkflowError> {
    let instruction = match input {
        JsonValue::String(value) if !value.trim().is_empty() => value.clone(),
        JsonValue::Object(object) => object
            .get("prompt")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Execute workflow node `{}`.", node.id)),
        _ => format!("Execute workflow node `{}`.", node.id),
    };
    let context = json!({
        "args": args,
        "input": input,
        "needs": needs,
        "outputSchema": output_schema,
    });
    let context = serde_json::to_string(&context).map_err(|error| {
        graph_engine_error(format!(
            "failed to encode node `{}` context: {error}",
            node.id
        ))
    })?;
    Ok(format!(
        "{instruction}\n\nWorkflow context (JSON):\n{context}"
    ))
}

pub(super) fn terminal_output(
    graph: &CompiledGraph,
    nodes: &[WorkflowNode],
    results: &[Option<JsonValue>],
) -> Result<JsonValue, WorkflowError> {
    if let [index] = graph.terminal_nodes() {
        return results
            .get(*index)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(|| graph_engine_error("terminal node completed without a result"));
    }

    let mut output = JsonMap::new();
    for index in graph.terminal_nodes() {
        let node = nodes
            .get(*index)
            .ok_or_else(|| graph_engine_error("terminal node is missing"))?;
        let value = results
            .get(*index)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(|| {
                graph_engine_error(format!(
                    "terminal node `{}` completed without a result",
                    node.id
                ))
            })?;
        output.insert(node.id.clone(), value);
    }
    Ok(JsonValue::Object(output))
}

pub(super) fn validate_compiled_graph(request: &WorkflowStartRequest) -> Result<(), WorkflowError> {
    if request.compiled.workflow.definition().spec.engine != WorkflowEngine::GraphV1 {
        return Err(graph_engine_error("compiled workflow is not graph/v1"));
    }
    if request.compiled.engine_revision != GRAPH_ENGINE_REVISION {
        return Err(graph_engine_error(format!(
            "unsupported graph engine revision `{}`",
            request.compiled.engine_revision
        )));
    }
    if request.compiled.artifact.as_ref() != request.compiled.workflow.identity().digest.as_bytes()
    {
        return Err(graph_engine_error(
            "compiled graph artifact does not match workflow source digest",
        ));
    }
    request
        .compiled
        .workflow
        .graph()
        .ok_or_else(|| graph_engine_error("compiled graph is unavailable"))?;
    Ok(())
}
