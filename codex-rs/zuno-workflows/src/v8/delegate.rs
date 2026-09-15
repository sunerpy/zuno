use super::WorkflowHost;
use super::WorkflowHostCall;
use super::WorkflowHostCallKind;
use super::engine_error;
use crate::WorkflowCallIdentity;
use crate::WorkflowError;
use crate::WorkflowStartRequest;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ToolDefinition;
use codex_code_mode::ToolInvocationFuture;
use codex_protocol::ToolName;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

pub(super) struct V8WorkflowDelegate {
    pub(super) host: Arc<dyn WorkflowHost>,
    pub(super) routes: BTreeSet<String>,
    pub(super) agents_started: Arc<AtomicU32>,
}

impl CodeModeSessionDelegate for V8WorkflowDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            if invocation.tool_kind != CodeModeToolKind::Function {
                return Err("workflow host calls must be functions".to_string());
            }
            let payload = invocation.input.unwrap_or(JsonValue::Null);
            let kind = match invocation.tool_name.name.as_str() {
                "workflow_agent" => {
                    let object = payload.as_object().ok_or("agent() requires an object")?;
                    let id = string_field(object, "id")?;
                    let route = string_field(object, "route")?;
                    let _ = string_field(object, "prompt")?;
                    if !self.routes.contains(route) {
                        return Err(format!("agent() references unknown route `{route}`"));
                    }
                    let identity = WorkflowCallIdentity::new(id, &payload)
                        .map_err(|error| error.to_string())?;
                    self.agents_started.fetch_add(1, Ordering::Relaxed);
                    let result = self
                        .host
                        .call(
                            WorkflowHostCall {
                                kind: WorkflowHostCallKind::Agent,
                                payload,
                                identity: Some(identity),
                            },
                            cancellation,
                        )
                        .await
                        .map_err(|error| error.to_string());
                    return result;
                }
                "workflow_phase" => {
                    validate_control_payload(&payload, "phase", &["title"], false)?;
                    WorkflowHostCallKind::Phase
                }
                "workflow_log" => {
                    validate_control_payload(&payload, "log", &["message"], false)?;
                    WorkflowHostCallKind::Log
                }
                "workflow_checkpoint" => {
                    validate_control_payload(&payload, "checkpoint", &["key"], true)?;
                    WorkflowHostCallKind::Checkpoint
                }
                name => return Err(format!("unknown workflow host function `{name}`")),
            };
            self.host
                .call(
                    WorkflowHostCall {
                        kind,
                        payload,
                        identity: None,
                    },
                    cancellation,
                )
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

fn validate_control_payload(
    payload: &JsonValue,
    operation: &str,
    required_strings: &[&str],
    require_value: bool,
) -> Result<(), String> {
    let object = payload
        .as_object()
        .ok_or_else(|| format!("{operation}() requires an object"))?;
    for field in required_strings {
        object
            .get(*field)
            .and_then(JsonValue::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("{operation}() requires non-empty `{field}`"))?;
    }
    if require_value && !object.contains_key("value") {
        return Err(format!("{operation}() requires `value`"));
    }
    Ok(())
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, JsonValue>,
    field: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(JsonValue::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("agent() requires non-empty `{field}`"))
}

pub(super) fn workflow_source(request: &WorkflowStartRequest) -> Result<String, WorkflowError> {
    let script = std::str::from_utf8(&request.compiled.artifact)
        .map_err(|error| engine_error(format!("compiled script is not UTF-8: {error}")))?;
    let args = serde_json::to_string(&request.args)
        .map_err(|error| engine_error(format!("failed to encode workflow args: {error}")))?;
    Ok(format!(
        r#"const args = Object.freeze({args});
const agent = async (prompt, options = {{}}) => tools.workflow_agent({{ ...options, prompt }});
const phase = async (title) => tools.workflow_phase({{ title }});
const log = async (message) => tools.workflow_log({{ message }});
const checkpoint = async (key, value) => tools.workflow_checkpoint({{ key, value }});
const parallel = async (thunks) => Promise.all(thunks.map((thunk) => thunk()));
const pipeline = async (items, ...stages) => Promise.all(items.map(async (item, index) => {{
  let value = item;
  for (const stage of stages) value = await stage(value, item, index);
  return value;
}}));
const __workflowValue = await (async () => {{
{script}
}})();
text(JSON.stringify(__workflowValue === undefined ? null : __workflowValue));"#
    ))
}

pub(super) fn host_tools() -> Vec<ToolDefinition> {
    [
        (
            "workflow_agent",
            "Start an agent through a configured workflow route.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "minLength": 1},
                    "route": {"type": "string", "minLength": 1},
                    "prompt": {"type": "string", "minLength": 1}
                },
                "required": ["id", "route", "prompt"],
                "additionalProperties": true
            }),
        ),
        (
            "workflow_phase",
            "Publish a workflow phase.",
            object_with_required_string("title"),
        ),
        (
            "workflow_log",
            "Publish a workflow log entry.",
            object_with_required_string("message"),
        ),
        (
            "workflow_checkpoint",
            "Persist a workflow checkpoint value.",
            json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "minLength": 1},
                    "value": {}
                },
                "required": ["key", "value"],
                "additionalProperties": false
            }),
        ),
    ]
    .into_iter()
    .map(|(name, description, input_schema)| ToolDefinition {
        name: name.to_string(),
        tool_name: ToolName::plain(name),
        description: description.to_string(),
        kind: CodeModeToolKind::Function,
        input_schema: Some(input_schema),
        output_schema: None,
    })
    .collect()
}

fn object_with_required_string(field: &str) -> JsonValue {
    json!({
        "type": "object",
        "properties": {field: {"type": "string", "minLength": 1}},
        "required": [field],
        "additionalProperties": false
    })
}
