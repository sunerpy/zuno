//! Tool dispatch: exact-name lookup, argument validation, permission gating, and execution.
//!
//! Every call passes through [`ToolRegistryDispatcher::dispatch`]. Keeping lookup and
//! miss recovery in one choke point is intentional: every model-visible tool ID must
//! name the registered implementation that permission policy and observability see.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Map, Value};
use tracing::Instrument as _;
use zuno_config::schema::permission::PermissionMode;
use zuno_llm::cache::McpToolStatus;
use zuno_observability::span;
use zuno_observability::tool::ToolLifecycle;
use zuno_permission::visibility::{is_tool_visible, permission_key};
use zuno_permission::{PermissionAction, Rule, evaluate};
use zuno_tool::{
    ACCEPT_LARGE_OUTPUT_KEY, INTENT_KEY, PermissionAsk, PermissionAsker, Tool,
    ToolConcurrencyPolicy, ToolContext, ToolDefinition, ToolOutput, ToolReplayPolicy,
};

use crate::hooks::{NoopHooks, PermissionHookDecision, ToolHooks};
use crate::r#loop::{
    AvailableTools, DispatchRequest, PreparedToolDispatch, ToolBlockKind, ToolDispatchResult,
    ToolDispatcher,
};

/// Executable tools plus the policy collaborators needed at the dispatch boundary.
pub struct ToolRegistryDispatcher {
    tools: Vec<Arc<dyn Tool>>,
    rules: Arc<[Rule]>,
    approval: Arc<dyn PermissionAsker>,
    authorization: AuthorizationPolicy,
    mcp_status: McpToolStatus,
    hooks: Arc<dyn ToolHooks>,
}

/// Cross-cutting authorization policy applied after explicit deny rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AuthorizationPolicy {
    /// Configured rules and permission hooks decide whether a prompt is needed.
    #[default]
    Standard,
    /// Every side-effecting invocation requires a fresh attached-user decision.
    Strict,
    /// Skip HITL prompts while keeping explicit deny rules terminal.
    AllowAll,
}

impl AuthorizationPolicy {
    /// Resolve the typed configuration mode at the engine boundary.
    #[must_use]
    pub const fn from_mode(mode: PermissionMode) -> Self {
        match mode {
            PermissionMode::Standard => Self::Standard,
            PermissionMode::Strict => Self::Strict,
            PermissionMode::AllowAll => Self::AllowAll,
        }
    }

    /// Resolve the configuration boolean without leaking config types into the engine.
    #[must_use]
    pub const fn from_strict(strict: bool) -> Self {
        if strict { Self::Strict } else { Self::Standard }
    }

    const fn is_strict(self) -> bool {
        matches!(self, Self::Strict)
    }

    const fn is_allow_all(self) -> bool {
        matches!(self, Self::AllowAll)
    }
}

impl ToolRegistryDispatcher {
    /// Builds a dispatcher over an already assembled registry.
    ///
    /// Registry assembly and model-conditional exposure belong to `zuno-tools`; this
    /// type only applies permission-based visibility and executes the supplied set.
    #[must_use]
    pub fn new(
        tools: Vec<Arc<dyn Tool>>,
        rules: Vec<Rule>,
        approval: Arc<dyn PermissionAsker>,
        authorization: AuthorizationPolicy,
        mcp_status: McpToolStatus,
    ) -> Self {
        let rules: Arc<[Rule]> = rules.into();
        Self {
            tools,
            rules,
            approval,
            authorization,
            mcp_status,
            hooks: Arc::new(NoopHooks),
        }
    }

    #[must_use]
    pub fn with_hooks(mut self, hooks: Arc<dyn ToolHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    fn visible_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|tool| is_tool_visible(tool.id(), &self.rules))
            .map(|tool| tool.definition())
            .collect()
    }

    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.id() == name)
            .map(Arc::clone)
    }
}

#[async_trait]
impl ToolDispatcher for ToolRegistryDispatcher {
    fn available_tools(&self) -> AvailableTools {
        AvailableTools::new(self.visible_definitions(), self.mcp_status)
    }

    fn concurrency_policy(&self, request: &DispatchRequest) -> ToolConcurrencyPolicy {
        self.tool(&request.call.name)
            .filter(|_| {
                request
                    .available_tools
                    .iter()
                    .any(|definition| definition.id == request.call.name)
            })
            .map_or(ToolConcurrencyPolicy::Exclusive, |tool| {
                tool.concurrency_policy()
            })
    }

    async fn prepare(&self, mut request: DispatchRequest) -> PreparedToolDispatch {
        let requested_name = request.call.name.clone();
        let observation = DispatchObservation::new(requested_name.clone(), request.call.id.clone());
        let resolved_name = requested_name.as_str();
        let available = available_names(&request.available_tools);

        let Some(tool) = self.tool(resolved_name).filter(|_| {
            request
                .available_tools
                .iter()
                .any(|definition| definition.id == resolved_name)
        }) else {
            return observed_ready(
                observation,
                unknown_tool_result(&requested_name, &available),
            );
        };
        let replay_policy = tool.replay_policy();
        let interrupt = request.interrupt.clone();
        if interrupt.is_set() {
            return observed_ready(
                observation,
                error_result(
                    resolved_name,
                    format!("Tool `{resolved_name}` was interrupted before it started."),
                ),
            );
        }

        let before = tokio::select! {
            biased;
            () = interrupt.notified() => {
                return observed_ready(
                    observation,
                    error_result(
                        resolved_name,
                        format!("Tool `{resolved_name}` was interrupted before it started."),
                    ),
                );
            }
            result = self.hooks.before(
                resolved_name,
                &request.session_id,
                &request.call.id,
                &mut request.call.input,
            ) => result,
        };
        if let Err(error) = before {
            return observed_ready(observation, error_result(resolved_name, error));
        }

        if let Some(input_error) = &request.call.input_error {
            tracing::warn!(
                tool = resolved_name,
                call_id = %request.call.id,
                error = input_error,
                "provider emitted malformed tool arguments"
            );
            return observed_ready(
                observation,
                blocked_result(
                    resolved_name,
                    format!("Malformed arguments for tool `{resolved_name}`: {input_error}"),
                    ToolBlockKind::InvalidArguments,
                ),
            );
        }

        let definition = tool.definition();
        if let Err(error) = validate_arguments(&definition.parameters, &request.call.input) {
            return observed_ready(
                observation,
                blocked_result(
                    resolved_name,
                    format!("Invalid arguments for tool `{resolved_name}`: {error}"),
                    ToolBlockKind::InvalidArguments,
                ),
            );
        }

        let ask = permission_ask(resolved_name, &request.call.input)
            .with_tool_effect(tool.effect(&request.call.input));
        let permission_request = ask.clone().into_request(
            format!("per_{}", request.call.id),
            &request.session_id,
            Some(zuno_permission::ToolCall {
                message_id: request.message_id.clone(),
                call_id: request.call.id.clone(),
            }),
        );
        let plugin_permission = match tokio::select! {
            biased;
            () = interrupt.notified() => {
                return observed_ready(
                    observation,
                    error_result(
                        resolved_name,
                        format!("Tool `{resolved_name}` was interrupted before permission."),
                    ),
                );
            }
            result = self.hooks.permission(&permission_request) => result,
        } {
            Ok(decision) => decision,
            Err(error) => {
                return observed_ready(observation, error_result(resolved_name, error));
            }
        };
        if plugin_permission == PermissionHookDecision::Deny {
            return observed_ready(
                observation,
                blocked_result(
                    resolved_name,
                    format!("Tool `{resolved_name}` was denied by a plugin."),
                    ToolBlockKind::Denied,
                ),
            );
        }
        let permission = Arc::new(RulePermissionAsker::new(
            Arc::clone(&self.rules),
            Arc::clone(&self.approval),
            self.authorization,
        ));
        let gate = tokio::select! {
            biased;
            () = interrupt.notified() => {
                return observed_ready(
                    observation,
                    error_result(
                        resolved_name,
                        format!("Tool `{resolved_name}` was interrupted before permission."),
                    ),
                );
            }
            result = permission.gate(resolved_name, ask, plugin_permission) => result,
        };
        if let Err(error) = gate {
            return observed_ready(
                observation,
                tool_error_result(resolved_name, replay_policy, &error),
            );
        }

        let permission: Arc<dyn PermissionAsker> = permission;
        let context = ToolContext::new(
            request.session_id.clone(),
            request.message_id,
            request.call.id.clone(),
            request.agent,
            permission,
            Arc::new(interrupt.clone()),
        );
        let args = request.call.input.clone();
        let tool_name = resolved_name.to_owned();
        let session_id = request.session_id;
        let call_id = request.call.id;
        let hook_args = request.call.input;
        let hooks = Arc::clone(&self.hooks);
        let DispatchObservation {
            span,
            mut lifecycle,
        } = observation;
        PreparedToolDispatch::new(Box::pin(
            async move {
                lifecycle.running();
                let execution_span = tracing::Span::current();
                let mut execution = tokio::spawn(
                    async move { tool.invoke(args, context).await }.instrument(execution_span),
                );

                let mut result = tokio::select! {
                    biased;
                    joined = &mut execution => joined_result(&tool_name, replay_policy, joined),
                    () = interrupt.notified() => {
                        execution.abort();
                        let _cancelled = execution.await;
                        error_result(
                            &tool_name,
                            format!("Tool `{tool_name}` was interrupted before it completed."),
                        )
                    }
                };
                if let Err(error) = hooks
                    .after(
                        &tool_name,
                        &session_id,
                        &call_id,
                        &hook_args,
                        &mut result.output,
                    )
                    .await
                {
                    result = error_result(&tool_name, error);
                }
                finish_observation(lifecycle, &result);
                result
            }
            .instrument(span),
        ))
    }
}

struct DispatchObservation {
    span: tracing::Span,
    lifecycle: ToolLifecycle,
}

impl DispatchObservation {
    fn new(tool: String, call_id: String) -> Self {
        Self {
            span: span::tool_call(&tool, &call_id),
            lifecycle: ToolLifecycle::pending(tool, call_id),
        }
    }
}

fn observed_ready(
    observation: DispatchObservation,
    result: ToolDispatchResult,
) -> PreparedToolDispatch {
    let DispatchObservation { span, lifecycle } = observation;
    PreparedToolDispatch::new(Box::pin(
        async move {
            finish_observation(lifecycle, &result);
            result
        }
        .instrument(span),
    ))
}

fn finish_observation(lifecycle: ToolLifecycle, result: &ToolDispatchResult) {
    if let Some(blocked) = result.blocked {
        lifecycle.blocked(blocked.as_str());
    } else if result.is_error {
        lifecycle.errored("tool_result");
    } else {
        lifecycle.completed();
    }
}

struct RulePermissionAsker {
    rules: Arc<[Rule]>,
    approval: Arc<dyn PermissionAsker>,
    authorization: AuthorizationPolicy,
    approved_once: Mutex<BTreeSet<(String, String)>>,
    approved_permissions: Mutex<BTreeSet<String>>,
}

/// What the configured rules decided about one ask, before anyone is prompted.
enum RuleOutcome {
    Permitted,
    Denied,
    Pending(Vec<String>),
}

impl RulePermissionAsker {
    fn new(
        rules: Arc<[Rule]>,
        approval: Arc<dyn PermissionAsker>,
        authorization: AuthorizationPolicy,
    ) -> Self {
        Self {
            rules,
            approval,
            authorization,
            approved_once: Mutex::new(BTreeSet::new()),
            approved_permissions: Mutex::new(BTreeSet::new()),
        }
    }

    fn evaluate_patterns(&self, ask: &PermissionAsk) -> RuleOutcome {
        let permission_approved = self
            .approved_permissions
            .lock()
            .expect("approved permission lock")
            .contains(&ask.permission);
        let mut pending = Vec::new();
        for pattern in &ask.patterns {
            match evaluate(&ask.permission, pattern, &self.rules) {
                PermissionAction::Allow => {}
                PermissionAction::Deny => return RuleOutcome::Denied,
                PermissionAction::Ask => {
                    let approved = self
                        .approved_once
                        .lock()
                        .expect("approved permission lock")
                        .contains(&(ask.permission.clone(), pattern.clone()));
                    if !permission_approved && !approved {
                        pending.push(pattern.clone());
                    }
                }
            }
        }
        if pending.is_empty() {
            RuleOutcome::Permitted
        } else {
            RuleOutcome::Pending(pending)
        }
    }

    /// The dispatch-time gate, which the rule set always passes through.
    ///
    /// A plugin `Allow` may only resolve patterns the rules left at `ask`; it can
    /// never cross a pattern an explicit `deny` rule matched. Consulting the rules
    /// only when the plugin returned `Ask` is what let a plugin silently discard the
    /// user's written prohibition.
    async fn gate(
        &self,
        tool: &str,
        mut ask: PermissionAsk,
        plugin: PermissionHookDecision,
    ) -> Result<(), zuno_error::ToolError> {
        normalize_patterns(&mut ask);
        let outcome = self.evaluate_patterns(&ask);
        match outcome {
            RuleOutcome::Denied => Err(zuno_error::ToolError::Denied {
                tool: tool.to_owned(),
            }),
            _ if self.requires_manual(&ask) => self.prompt_manual(tool, ask).await,
            _ if self.authorization.is_allow_all() => Ok(()),
            RuleOutcome::Permitted => Ok(()),
            RuleOutcome::Pending(_) if plugin == PermissionHookDecision::Allow => Ok(()),
            RuleOutcome::Pending(pending) => self.prompt(tool, ask, pending).await,
        }
    }

    fn requires_manual(&self, ask: &PermissionAsk) -> bool {
        ask.manual
            || self.authorization.is_strict()
                && ask
                    .tool_effect
                    .is_some_and(zuno_tool::ToolEffect::requires_manual_approval)
    }

    async fn prompt_manual(
        &self,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), zuno_error::ToolError> {
        let permission = ask.permission.clone();
        self.approval.ask(tool, ask.require_manual()).await?;
        self.approved_permissions
            .lock()
            .expect("approved permission lock")
            .insert(permission);
        Ok(())
    }

    async fn prompt(
        &self,
        tool: &str,
        mut ask: PermissionAsk,
        pending: Vec<String>,
    ) -> Result<(), zuno_error::ToolError> {
        let approved_patterns = pending.clone();
        ask.patterns = pending;
        self.approval.ask(tool, ask.clone()).await?;
        let mut approved_once = self.approved_once.lock().expect("approved permission lock");
        approved_once.extend(
            approved_patterns
                .into_iter()
                .map(|pattern| (ask.permission.clone(), pattern)),
        );
        Ok(())
    }
}

fn normalize_patterns(ask: &mut PermissionAsk) {
    if ask.patterns.is_empty() {
        ask.patterns.push("*".to_owned());
    }
}

#[async_trait]
impl PermissionAsker for RulePermissionAsker {
    async fn ask(&self, tool: &str, mut ask: PermissionAsk) -> Result<(), zuno_error::ToolError> {
        normalize_patterns(&mut ask);
        let outcome = self.evaluate_patterns(&ask);
        match outcome {
            RuleOutcome::Denied => Err(zuno_error::ToolError::Denied {
                tool: tool.to_owned(),
            }),
            _ if self.requires_manual(&ask) => self.prompt_manual(tool, ask).await,
            RuleOutcome::Permitted => Ok(()),
            RuleOutcome::Pending(pending) => self.prompt(tool, ask, pending).await,
        }
    }
}

fn permission_ask(tool: &str, args: &Value) -> PermissionAsk {
    let permission = permission_key(tool).to_owned();
    let patterns = permission_patterns(tool, args);
    let mut metadata = Map::new();
    metadata.insert("arguments".to_owned(), args.clone());
    PermissionAsk {
        permission,
        always: patterns.clone(),
        patterns,
        metadata,
        ..PermissionAsk::default()
    }
}

/// Derive the permission resource from call arguments, never from the tool name alone.
#[must_use]
pub fn permission_patterns(tool: &str, args: &Value) -> Vec<String> {
    let patterns = match tool {
        "bash" => strings_at(args, &["command"]),
        "read" | "write" | "edit" => strings_at(args, &["filePath", "file_path", "path"]),
        "apply_patch" => patch_paths(args),
        "glob" | "grep" => strings_at(args, &["pattern", "query"]),
        "webfetch" => strings_at(args, &["url"]),
        "web_search" => strings_at(args, &["queries"]),
        "task" => strings_at(args, &["subagent_type", "subagentType"]),
        "skill" => strings_at(args, &["name"]),
        "read_mcp_resource" => strings_at(args, &["uri", "resource_name", "server"]),
        "list_mcp_resources" | "list_mcp_resource_templates" => strings_at(args, &["server"]),
        "plan_get" | "plan_update" | "todo_get" | "todo_update" | "question" | "invalid"
        | "plan_exit" | "lsp" | "execute" => {
            vec!["*".to_owned()]
        }
        _ => strings_at(
            args,
            &[
                "path",
                "filePath",
                "file_path",
                "url",
                "uri",
                "query",
                "pattern",
                "command",
                "name",
            ],
        ),
    };

    if patterns.is_empty() {
        vec![canonical_resource(args)]
    } else {
        deduplicate(patterns)
    }
}

fn strings_at(args: &Value, keys: &[&str]) -> Vec<String> {
    let Some(object) = args.as_object() else {
        return Vec::new();
    };
    keys.iter()
        .filter_map(|key| object.get(*key))
        .flat_map(|value| match value {
            Value::String(value) => vec![value.clone()],
            Value::Array(values) => values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        })
        .filter(|value| !value.is_empty())
        .collect()
}

fn patch_paths(args: &Value) -> Vec<String> {
    let Some(patch) = strings_at(args, &["patchText", "patch_text", "patch"])
        .into_iter()
        .next()
    else {
        return Vec::new();
    };
    patch
        .lines()
        .filter_map(|line| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .into_iter()
            .find_map(|prefix| line.strip_prefix(prefix))
        })
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

fn canonical_resource(args: &Value) -> String {
    fn normalize(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let normalized = object
                    .iter()
                    .filter(|(key, _)| {
                        key.as_str() != INTENT_KEY && key.as_str() != ACCEPT_LARGE_OUTPUT_KEY
                    })
                    .map(|(key, value)| (key.clone(), normalize(value)))
                    .collect::<std::collections::BTreeMap<_, _>>();
                Value::Object(normalized.into_iter().collect())
            }
            Value::Array(values) => Value::Array(values.iter().map(normalize).collect()),
            other => other.clone(),
        }
    }

    let normalized = normalize(args);
    match normalized {
        Value::Object(ref object) if object.is_empty() => "*".to_owned(),
        _ => normalized.to_string(),
    }
}

fn deduplicate(patterns: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    patterns
        .into_iter()
        .filter(|pattern| seen.insert(pattern.clone()))
        .collect()
}

fn validate_arguments(schema: &Value, args: &Value) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("tool schema is invalid: {error}"))?;
    let errors = validator
        .iter_errors(args)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn available_names(definitions: &[ToolDefinition]) -> Vec<String> {
    let mut names = definitions
        .iter()
        .map(|definition| definition.id.clone())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    names
}

fn unknown_tool_result(name: &str, available: &[String]) -> ToolDispatchResult {
    let candidates = available.iter().map(String::as_str).collect::<Vec<_>>();
    let suggestions = closest_tool_names(name, &candidates);
    let mut message = format!("Unknown tool: {name}.");
    if !suggestions.is_empty() {
        message.push_str(&format!(" Did you mean: {}?", suggestions.join(", ")));
    }
    message.push_str(&format!(" Available tools: {}.", available.join(", ")));
    blocked_result(name, message, ToolBlockKind::Unavailable)
}

fn closest_tool_names(name: &str, available: &[&str]) -> Vec<String> {
    let needle = name.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut scored = available
        .iter()
        .filter_map(|candidate| {
            let hay = candidate.to_ascii_lowercase();
            let score = if hay == needle {
                0
            } else if hay.starts_with(&needle) || needle.starts_with(&hay) {
                1
            } else if hay.contains(&needle) || needle.contains(&hay) {
                2
            } else {
                let distance = levenshtein(&needle, &hay);
                let threshold = (hay.len().max(needle.len()) / 3).max(2);
                if distance > threshold {
                    return None;
                }
                3 + distance
            };
            Some((score, *candidate))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
    scored
        .into_iter()
        .take(3)
        .map(|(_, candidate)| candidate.to_owned())
        .collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    if left.is_empty() {
        return right.len();
    }
    if right.is_empty() {
        return left.len();
    }

    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_char) in left.iter().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right.iter().enumerate() {
            let substitution = usize::from(left_char != right_char);
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

fn joined_result(
    tool: &str,
    replay_policy: ToolReplayPolicy,
    joined: Result<Result<ToolOutput, zuno_error::ToolError>, tokio::task::JoinError>,
) -> ToolDispatchResult {
    match joined {
        Ok(Ok(output)) => ToolDispatchResult::success(output),
        Ok(Err(error)) => tool_error_result(tool, replay_policy, &error),
        Err(error) => error_result(tool, format!("Tool `{tool}` task failed: {error}")),
    }
}

/// Render a failed call for the model: its category, then every cause beneath it.
///
/// # Why the whole chain and not the first cause
///
/// A [`zuno_error::ToolError`] classifies and hangs the detail off a `#[source]`, and
/// a chain is routinely deeper than one link: an MCP proxy reports `tool X failed`
/// wrapping the server's rejection wrapping the transport error that caused it.
/// Unwrapping exactly one level therefore stops one link short of the reason on every
/// failure whose cause is itself wrapped, and hands the model a message naming a
/// category it cannot act on. [`zuno_error::source::describe`] walks the chain once
/// for every variant, which is why it is called here rather than reimplemented — its
/// own documentation records that reaching for a cause by hand at a call site fixes
/// that site and leaves the next one broken.
fn tool_error_result(
    tool: &str,
    replay_policy: ToolReplayPolicy,
    error: &zuno_error::ToolError,
) -> ToolDispatchResult {
    let mut message = zuno_error::source::describe(error);
    if !error.is_retryable() {
        let blocked = match error {
            zuno_error::ToolError::Denied { .. } => Some(ToolBlockKind::Denied),
            zuno_error::ToolError::InvalidArgs { .. } => Some(ToolBlockKind::InvalidArguments),
            zuno_error::ToolError::NotFound { .. } => Some(ToolBlockKind::Unavailable),
            zuno_error::ToolError::Timeout { .. }
            | zuno_error::ToolError::Transient { .. }
            | zuno_error::ToolError::Failed { .. }
            | zuno_error::ToolError::Uncertain { .. } => None,
        };
        if let Some(kind) = blocked {
            return blocked_result(tool, message, kind);
        }
        if matches!(error, zuno_error::ToolError::Uncertain { .. }) {
            message.push_str(
                "\n\nRecovery: this call changed authoritative state before losing its final outcome. Inspect the listed paths and continue from what is actually on disk; do not replay the call mechanically.",
            );
        }
        return error_result(tool, message);
    }
    match replay_policy {
        ToolReplayPolicy::Safe => message.push_str(
            "\n\nRecovery: this tool explicitly permits replay after backoff. Do not retry it in a tight loop; the active goal will schedule another turn.",
        ),
        ToolReplayPolicy::Never => message.push_str(
            "\n\nRecovery: this tool may have produced a side effect before its result was lost. Do not replay the call until authoritative external state proves it did not complete.",
        ),
    }
    ToolDispatchResult::retryable_error(
        ToolOutput::text(format!("{tool} error"), message),
        crate::r#loop::ToolFailureRecovery {
            tool: tool.to_owned(),
            replay_policy,
            retry_after: error.retry_after(),
        },
    )
}

fn error_result(tool: &str, message: String) -> ToolDispatchResult {
    ToolDispatchResult::error(ToolOutput::text(format!("{tool} error"), message))
}

fn blocked_result(tool: &str, message: String, kind: ToolBlockKind) -> ToolDispatchResult {
    ToolDispatchResult::blocked(ToolOutput::text(format!("{tool} blocked"), message), kind)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn argument_patterns_cover_builtin_resource_shapes() {
        assert_eq!(
            permission_patterns("bash", &serde_json::json!({"command": "git push"})),
            ["git push"]
        );
        assert_eq!(
            permission_patterns("read", &serde_json::json!({"filePath": "/etc/hosts"})),
            ["/etc/hosts"]
        );
        assert_eq!(
            permission_patterns(
                "webfetch",
                &serde_json::json!({"url": "https://example.com"})
            ),
            ["https://example.com"]
        );
        assert_eq!(
            permission_patterns("task", &serde_json::json!({"subagent_type": "explore"})),
            ["explore"]
        );
    }

    #[test]
    fn patch_permission_is_per_affected_path() {
        let args = serde_json::json!({
            "patchText": "*** Begin Patch\n*** Add File: src/new.rs\n+x\n*** Update File: src/lib.rs\n*** Move to: src/main.rs\n*** End Patch"
        });
        assert_eq!(
            permission_patterns("apply_patch", &args),
            ["src/new.rs", "src/lib.rs", "src/main.rs"]
        );
    }

    #[test]
    fn generic_resource_is_stable_and_ignores_cross_cutting_arguments() {
        let first = serde_json::json!({"z": 1, "a": 2, "intent": "first"});
        let second = serde_json::json!({"a": 2, "z": 1, "intent": "second"});
        assert_eq!(permission_patterns("plugin", &first), [r#"{"a":2,"z":1}"#]);
        assert_eq!(
            permission_patterns("plugin", &first),
            permission_patterns("plugin", &second)
        );
    }

    #[test]
    fn suggester_copies_reference_ranking_and_cutoff() {
        let suggestions =
            closest_tool_names("ToolSerch", &["bash", "tool_search", "web_search", "todo"]);
        assert_eq!(suggestions.first().map(String::as_str), Some("tool_search"));
        assert!(!suggestions.contains(&"bash".to_owned()));
        assert!(suggestions.len() <= 3);
    }

    #[test]
    fn retryable_tool_errors_preserve_replay_policy_and_peer_delay() {
        let retry_after = Duration::from_secs(7);
        let error = zuno_error::ToolError::Transient {
            tool: "web_search".to_owned(),
            retry_after: Some(retry_after),
            source: Box::new(std::io::Error::other("HTTP 429")),
        };

        let result = tool_error_result("web_search", ToolReplayPolicy::Safe, &error);

        assert_eq!(
            result.recovery,
            Some(crate::r#loop::ToolFailureRecovery {
                tool: "web_search".to_owned(),
                replay_policy: ToolReplayPolicy::Safe,
                retry_after: Some(retry_after),
            })
        );
        assert!(result.output.output.contains("permits replay"));
    }

    #[test]
    fn retryable_mutations_require_state_verification_before_replay() {
        let error = zuno_error::ToolError::Timeout {
            tool: "bash".to_owned(),
            elapsed: Duration::from_secs(120),
        };

        let result = tool_error_result("bash", ToolReplayPolicy::Never, &error);

        assert_eq!(
            result
                .recovery
                .as_ref()
                .map(|recovery| recovery.replay_policy),
            Some(ToolReplayPolicy::Never)
        );
        assert!(
            result
                .output
                .output
                .contains("authoritative external state")
        );
    }
}
