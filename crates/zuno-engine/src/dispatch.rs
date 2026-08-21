//! Tool dispatch: name compatibility, argument validation, permission gating, and execution.
//!
//! Every call passes through [`ToolRegistryDispatcher::dispatch`]. Keeping lookup and
//! miss recovery in one choke point is intentional: a second fallback path can execute
//! an alias without applying the same permission policy, or report an unknown name
//! without the recovery information the model needs.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use zuno_llm::cache::McpToolStatus;
use zuno_permission::visibility::{is_tool_visible, permission_key};
use zuno_permission::{PermissionAction, Rule, evaluate};
use zuno_tool::{
    ACCEPT_LARGE_OUTPUT_KEY, INTENT_KEY, PermissionAsk, PermissionAsker, Tool, ToolContext,
    ToolDefinition, ToolOutput,
};

use crate::hooks::{NoopHooks, PermissionHookDecision, ToolHooks};
use crate::interrupt::BackgroundToolSignal;
use crate::r#loop::{AvailableTools, DispatchRequest, ToolDispatchResult, ToolDispatcher};

/// Time afforded to a tool to finish normally after a background request.
///
/// This matches the reference runtime's handoff window. A short grace avoids
/// reporting a call as detached when it was already about to finish, while still
/// returning control promptly for a genuinely long-running command.
pub const BACKGROUND_GRACE_PERIOD: Duration = Duration::from_millis(750);

/// Executable tools plus the policy collaborators needed at the dispatch boundary.
pub struct ToolRegistryDispatcher {
    tools: Vec<Arc<dyn Tool>>,
    rules: Arc<[Rule]>,
    approval: Arc<dyn PermissionAsker>,
    background_tool: BackgroundToolSignal,
    mcp_status: McpToolStatus,
    hooks: Arc<dyn ToolHooks>,
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
        background_tool: BackgroundToolSignal,
        mcp_status: McpToolStatus,
    ) -> Self {
        let rules: Arc<[Rule]> = rules.into();
        Self {
            tools,
            rules,
            approval,
            background_tool,
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

    async fn dispatch(&self, mut request: DispatchRequest) -> ToolDispatchResult {
        let requested_name = request.call.name.clone();
        let resolved_name = resolve_tool_name(&requested_name);
        let available = available_names(&request.available_tools);

        let Some(tool) = self.tool(resolved_name).filter(|_| {
            request
                .available_tools
                .iter()
                .any(|definition| definition.id == resolved_name)
        }) else {
            return unknown_tool_result(&requested_name, &available);
        };

        if let Err(error) = self
            .hooks
            .before(
                resolved_name,
                &request.session_id,
                &request.call.id,
                &mut request.call.input,
            )
            .await
        {
            return error_result(resolved_name, error);
        }

        if let Some(input_error) = &request.call.input_error {
            return error_result(
                resolved_name,
                format!(
                    "Malformed arguments for tool `{resolved_name}`: {input_error}. Raw input: {}",
                    request.call.raw_input
                ),
            );
        }

        let definition = tool.definition();
        if let Err(error) = validate_arguments(&definition.parameters, &request.call.input) {
            return error_result(
                resolved_name,
                format!("Invalid arguments for tool `{resolved_name}`: {error}"),
            );
        }

        let ask = permission_ask(resolved_name, &request.call.input);
        let permission_request = ask.clone().into_request(
            format!("per_{}", request.call.id),
            &request.session_id,
            Some(zuno_permission::ToolCall {
                message_id: request.message_id.clone(),
                call_id: request.call.id.clone(),
            }),
        );
        let plugin_permission = match self.hooks.permission(&permission_request).await {
            Ok(decision) => decision,
            Err(error) => return error_result(resolved_name, error),
        };
        if plugin_permission == PermissionHookDecision::Deny {
            return error_result(
                resolved_name,
                format!("Tool `{resolved_name}` was denied by a plugin."),
            );
        }
        let permission = Arc::new(RulePermissionAsker::new(
            Arc::clone(&self.rules),
            Arc::clone(&self.approval),
        ));
        if let Err(error) = permission.gate(resolved_name, ask, plugin_permission).await {
            return tool_error_result(resolved_name, &error);
        }

        let background_epoch = self.background_tool.epoch();
        let _reset_applied = self.background_tool.reset_if_epoch(background_epoch);

        let interrupt = request.interrupt.clone();
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
        let mut execution = tokio::spawn(async move { tool.invoke(args, context).await });

        let mut result = tokio::select! {
            biased;
            joined = &mut execution => joined_result(&tool_name, joined),
            () = self.background_tool.notified() => {
                match tokio::time::timeout(BACKGROUND_GRACE_PERIOD, &mut execution).await {
                    Ok(joined) => joined_result(&tool_name, joined),
                    Err(_) => ToolDispatchResult::success(ToolOutput::text(
                        format!("{tool_name} running in background"),
                        format!(
                            "Tool `{tool_name}` is still running in the background after the {}ms grace period.",
                            BACKGROUND_GRACE_PERIOD.as_millis()
                        ),
                    )),
                }
            }
            () = interrupt.notified() => {
                execution.abort();
                error_result(
                    &tool_name,
                    format!("Tool `{tool_name}` was interrupted before it completed."),
                )
            }
        };
        if let Err(error) = self
            .hooks
            .after(
                resolved_name,
                &request.session_id,
                &request.call.id,
                &request.call.input,
                &mut result.output,
            )
            .await
        {
            return error_result(resolved_name, error);
        }
        result
    }
}

/// Normalize provider, cross-agent, and historical tool names to registry IDs.
///
/// Transport namespacing is stripped before alias matching so nested calls such as
/// `functions.shell_exec` follow exactly the same path as top-level `shell_exec`.
#[must_use]
pub fn resolve_tool_name(name: &str) -> &str {
    let name = name.strip_prefix("functions.").unwrap_or(name);
    match name {
        "communicate" => "task",
        "task_runner" | "subagent" => "task",
        "launch" => "open",
        "shell" | "shell_exec" => "bash",
        "read_file" | "file_read" => "read",
        "write_file" | "file_write" => "write",
        "edit_file" | "file_edit" => "edit",
        "file_grep" => "grep",
        "skill_manage" => "skill",
        "discover_tools" => "integration_tools",
        "todoread" | "todo_read" | "todo_write" | "todos" | "todo" => "todowrite",
        "Bash" => "bash",
        "Read" => "read",
        "Write" => "write",
        "Edit" => "edit",
        "Grep" => "grep",
        "Agent" | "Task" => "task",
        "Skill" => "skill",
        "WebFetch" => "webfetch",
        "WebSearch" => "web_search",
        "TodoWrite" => "todowrite",
        "ApplyPatch" => "apply_patch",
        "Question" => "question",
        "PlanExit" => "plan_exit",
        "Lsp" => "lsp",
        "Execute" => "execute",
        "ScheduleWakeup" => "schedule",
        other => other,
    }
}

struct RulePermissionAsker {
    rules: Arc<[Rule]>,
    approval: Arc<dyn PermissionAsker>,
    approved_once: Mutex<BTreeSet<(String, String)>>,
}

/// What the configured rules decided about one ask, before anyone is prompted.
enum RuleOutcome {
    Permitted,
    Denied,
    Pending(Vec<String>),
}

impl RulePermissionAsker {
    fn new(rules: Arc<[Rule]>, approval: Arc<dyn PermissionAsker>) -> Self {
        Self {
            rules,
            approval,
            approved_once: Mutex::new(BTreeSet::new()),
        }
    }

    fn evaluate_patterns(&self, ask: &PermissionAsk) -> RuleOutcome {
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
                    if !approved {
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
        match self.evaluate_patterns(&ask) {
            RuleOutcome::Denied => Err(zuno_error::ToolError::Denied {
                tool: tool.to_owned(),
            }),
            RuleOutcome::Permitted => Ok(()),
            RuleOutcome::Pending(_) if plugin == PermissionHookDecision::Allow => Ok(()),
            RuleOutcome::Pending(pending) => self.prompt(tool, ask, pending).await,
        }
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
        match self.evaluate_patterns(&ask) {
            RuleOutcome::Denied => Err(zuno_error::ToolError::Denied {
                tool: tool.to_owned(),
            }),
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
        "todowrite" | "question" | "invalid" | "plan_exit" | "lsp" | "execute" => {
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
    error_result(name, message)
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
    joined: Result<Result<ToolOutput, zuno_error::ToolError>, tokio::task::JoinError>,
) -> ToolDispatchResult {
    match joined {
        Ok(Ok(output)) => ToolDispatchResult::success(output),
        Ok(Err(error)) => tool_error_result(tool, &error),
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
fn tool_error_result(tool: &str, error: &zuno_error::ToolError) -> ToolDispatchResult {
    error_result(tool, zuno_error::source::describe(error))
}

fn error_result(tool: &str, message: String) -> ToolDispatchResult {
    ToolDispatchResult::error(ToolOutput::text(format!("{tool} error"), message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_keeps_all_compatibility_aliases_at_one_choke_point() {
        let aliases = [
            ("functions.bash", "bash"),
            ("functions.shell_exec", "bash"),
            ("communicate", "task"),
            ("task_runner", "task"),
            ("subagent", "task"),
            ("launch", "open"),
            ("read_file", "read"),
            ("file_read", "read"),
            ("write_file", "write"),
            ("file_write", "write"),
            ("edit_file", "edit"),
            ("file_edit", "edit"),
            ("file_grep", "grep"),
            ("skill_manage", "skill"),
            ("discover_tools", "integration_tools"),
            ("todos", "todowrite"),
            ("Bash", "bash"),
            ("Read", "read"),
            ("Write", "write"),
            ("Edit", "edit"),
            ("Grep", "grep"),
            ("Agent", "task"),
            ("Skill", "skill"),
            ("ApplyPatch", "apply_patch"),
            ("ScheduleWakeup", "schedule"),
        ];
        for (alias, expected) in aliases {
            assert_eq!(resolve_tool_name(alias), expected, "alias {alias}");
        }
        assert_eq!(
            resolve_tool_name("mcp.functions.bash"),
            "mcp.functions.bash"
        );
    }

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
}
