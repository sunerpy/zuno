//! Tool dispatch: exact-name lookup, argument validation, permission gating, and execution.
//!
//! Every call passes through [`ToolRegistryDispatcher::dispatch`]. Keeping lookup and
//! miss recovery in one choke point is intentional: every model-visible tool ID must
//! name the registered implementation that permission policy and observability see.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tracing::Instrument as _;
use zuno_config::schema::permission::PermissionMode;
use zuno_llm::cache::McpToolStatus;
use zuno_observability::span;
use zuno_observability::tool::ToolLifecycle;
use zuno_permission::visibility::{is_tool_visible, permission_key};
use zuno_permission::{PermissionAction, Rule, evaluate};
use zuno_tool::{
    ACCEPT_LARGE_OUTPUT_KEY, INTENT_KEY, METADATA_MUTATION_CONFLICT_KEY,
    MutationConflictPresentation, PermissionAsk, PermissionAsker, PermissionOrigin, Tool,
    ToolConcurrencyPolicy, ToolContext, ToolDefinition, ToolOutput, ToolReplayPolicy,
    ToolResultPresentation, UncertainMutationPresentation,
};

use crate::deferred_tools::DeferredToolCatalog;
use crate::hooks::{NoopHooks, PermissionHookDecision, ToolHooks};
use crate::r#loop::{
    AvailableTools, DispatchRequest, PreparedToolDispatch, ToolBlockKind, ToolDispatchResult,
    ToolDispatcher, ToolInterruption, UncertainOutcome,
};

const TOOL_INTERRUPT_SETTLE_GRACE: Duration = Duration::from_secs(2);

/// The metadata key a tool uses to say what its cancelled call still proved.
///
/// A tool that observes cancellation can settle in two ways that are not equally safe.
/// It can hand back a decided outcome — the process had already exited, the write had
/// already been committed — or it can hand back the fragment of a call that was stopped
/// partway through a side effect. Only the tool knows which, so it says so here and the
/// dispatcher reads it; the alternative is the dispatcher guessing, which is how every
/// cooperative cancellation came to be reported as certain.
///
/// The producer side is `zuno_tools::shell::METADATA_CANCELLATION_KEY`. This crate does
/// not depend on `zuno-tools` — dispatch is the layer the tools are handed to — so the
/// spelling is pinned by a test on each side rather than shared through an import.
const CANCELLATION_METADATA_KEY: &str = "cancellation";

/// The metadata key under which [`interrupted_result`] records the resolved verdict.
///
/// The durable record of one interrupted call: its mode, whether the grace window
/// expired, the certainty the dispatcher resolved, and the window it allowed.
pub(crate) const INTERRUPTION_METADATA_KEY: &str = "interruption";

/// Whether a settled result says its cancelled outcome needs authoritative inspection.
///
/// Absent, malformed, or unset metadata answers `false`: a tool that makes no claim
/// keeps the reading cooperative cancellation has always had, so this cannot turn every
/// cancellation uncertain by default.
fn cancellation_is_uncertain(output: &ToolOutput) -> bool {
    output
        .metadata
        .get(CANCELLATION_METADATA_KEY)
        .and_then(|facts| facts.get("uncertain"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The certainty verdict this dispatch result recorded for an interrupted call.
///
/// The dispatcher resolves the verdict once, against the tool's own claim, and writes it
/// to the result's durable metadata. This is how the runtime reads it back so the live
/// event publishes the same fact the durable record keeps, instead of every surface
/// re-deriving certainty from the interruption mode and disagreeing with storage.
///
/// `None` when the result carries no readable verdict — a result that was not
/// interrupted, or one produced before the dispatcher recorded one. The caller falls
/// back to the mode, which is the reading those results were written under.
#[must_use]
pub fn recorded_interruption_uncertainty(result: &ToolDispatchResult) -> Option<bool> {
    result
        .output
        .metadata
        .get(INTERRUPTION_METADATA_KEY)?
        .get("uncertain")?
        .as_bool()
}

pub use crate::deferred_tools::TOOL_SEARCH_ID;

/// Executable tools plus the policy collaborators needed at the dispatch boundary.
pub struct ToolRegistryDispatcher {
    tools: Vec<Arc<dyn Tool>>,
    rules: Arc<[Rule]>,
    approval: Arc<dyn PermissionAsker>,
    authorization: AuthorizationPolicy,
    mcp_status: McpToolStatus,
    hooks: Arc<dyn ToolHooks>,
    deferred: Option<Arc<DeferredToolCatalog>>,
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
            deferred: None,
        }
    }

    /// Keep `ids` executable while withholding their schemas until `tool_search`
    /// selects matching metadata.
    ///
    /// The supplied set has already passed Agent allowlists, parent authority, and
    /// registry suppression. Permission-hidden tools are excluded again here so
    /// discovery cannot reveal a capability this Agent is not allowed to see.
    #[must_use]
    pub fn with_deferred_tools(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        let ids = ids.into_iter().collect::<BTreeSet<_>>();
        if ids.is_empty() || self.tools.iter().any(|tool| tool.id() == TOOL_SEARCH_ID) {
            return self;
        }
        let candidates = self
            .tools
            .iter()
            .filter(|tool| ids.contains(tool.id()) && is_tool_visible(tool.id(), &self.rules))
            .map(|tool| tool.definition())
            .collect();
        let Some(catalog) = DeferredToolCatalog::new(candidates) else {
            return self;
        };
        self.tools.push(catalog.search_tool());
        self.deferred = Some(catalog);
        self
    }

    #[must_use]
    pub fn with_hooks(mut self, hooks: Arc<dyn ToolHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Whether one tool is present after the effective permission visibility rules.
    ///
    /// Host policy uses this before a provider request to avoid promising durable
    /// state the active Agent cannot actually update.
    #[must_use]
    pub fn has_visible_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| {
            tool.id() == name
                && is_tool_visible(tool.id(), &self.rules)
                && self
                    .deferred
                    .as_ref()
                    .is_none_or(|catalog| !catalog.contains(name) || catalog.is_exposed(name))
        })
    }

    fn visible_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|tool| {
                is_tool_visible(tool.id(), &self.rules)
                    && self.deferred.as_ref().is_none_or(|catalog| {
                        !catalog.contains(tool.id()) || catalog.is_exposed(tool.id())
                    })
            })
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
        AvailableTools::new(self.visible_definitions(), self.mcp_status).with_revision(
            self.deferred
                .as_ref()
                .map_or(0, |catalog| catalog.revision()),
        )
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
                tool.concurrency_policy_for(&request.call.input)
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
        let scheduled_concurrency = tool.concurrency_policy_for(&request.call.input);
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

        let effective_concurrency = tool.concurrency_policy_for(&request.call.input);
        if effective_concurrency != scheduled_concurrency {
            return observed_ready(
                observation,
                blocked_result(
                    resolved_name,
                    format!(
                        "Arguments for tool `{resolved_name}` changed its concurrency policy \
                         after scheduling; the call was refused before execution."
                    ),
                    ToolBlockKind::InvalidArguments,
                ),
            );
        }
        let replay_policy = tool.replay_policy_for(&request.call.input);
        let ask = permission_ask(resolved_name, &request.call.input)
            .with_tool_effect(tool.effect(&request.call.input));
        let permission = Arc::new(RulePermissionAsker::new(
            Arc::clone(&self.rules),
            Arc::clone(&self.approval),
            self.authorization,
        ));
        let permission_for_context: Arc<dyn PermissionAsker> = permission.clone();
        let mut context = ToolContext::new(
            request.session_id.clone(),
            request.message_id.clone(),
            request.call.id.clone(),
            request.agent.clone(),
            permission_for_context,
            Arc::new(interrupt.clone()),
        );
        if let Some(snapshot) = request.orchestration_snapshot.as_ref() {
            context = context.with_orchestration_snapshot(Arc::clone(snapshot));
        }
        let permission_request = context
            .permission_origin()
            .into_request(format!("per_{}", request.call.id), ask.clone());
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
            result = permission.gate(
                context.permission_origin(),
                resolved_name,
                ask,
                plugin_permission,
            ) => result,
        };
        if let Err(error) = gate {
            return observed_ready(
                observation,
                tool_error_result(resolved_name, replay_policy, &error),
            );
        }

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
                        match tokio::time::timeout(
                            TOOL_INTERRUPT_SETTLE_GRACE,
                            &mut execution,
                        )
                        .await
                        {
                            Ok(settled) => interrupted_result(
                                &tool_name,
                                ToolInterruption::Cooperative,
                                Some(joined_result(&tool_name, replay_policy, settled)),
                            ),
                            Err(_elapsed) => {
                                execution.abort();
                                interrupted_result(
                                    &tool_name,
                                    ToolInterruption::Forced,
                                    None,
                                )
                            }
                        }
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
                    // The tool has already run, so whatever it changed is real whether
                    // or not a plugin managed to post-process the output. Rewriting a
                    // settled result into a bare error would tell the model the effect
                    // never happened and invite it to repeat a side effect; the result
                    // keeps its own status and the hook failure travels with it.
                    tracing::warn!(
                        target: "zuno_engine::dispatch",
                        tool = %tool_name,
                        call_id = %call_id,
                        error = %error,
                        "tool after-hook failed; keeping the settled tool result"
                    );
                    result
                        .output
                        .metadata
                        .insert("afterHookError".to_owned(), json!({ "message": error }));
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
        origin: PermissionOrigin<'_>,
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
            _ if self.authorization.is_allow_all() => Ok(()),
            _ if self.requires_manual(&ask) => self.prompt_manual(origin, tool, ask).await,
            RuleOutcome::Permitted => Ok(()),
            RuleOutcome::Pending(_) if plugin == PermissionHookDecision::Allow => Ok(()),
            RuleOutcome::Pending(pending) => self.prompt(origin, tool, ask, pending).await,
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
        origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), zuno_error::ToolError> {
        let permission = ask.permission.clone();
        self.approval
            .ask(origin, tool, ask.require_manual())
            .await?;
        self.approved_permissions
            .lock()
            .expect("approved permission lock")
            .insert(permission);
        Ok(())
    }

    async fn prompt(
        &self,
        origin: PermissionOrigin<'_>,
        tool: &str,
        mut ask: PermissionAsk,
        pending: Vec<String>,
    ) -> Result<(), zuno_error::ToolError> {
        let approved_patterns = pending.clone();
        ask.patterns = pending;
        self.approval.ask(origin, tool, ask.clone()).await?;
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
    async fn ask(
        &self,
        origin: PermissionOrigin<'_>,
        tool: &str,
        mut ask: PermissionAsk,
    ) -> Result<(), zuno_error::ToolError> {
        normalize_patterns(&mut ask);
        let outcome = self.evaluate_patterns(&ask);
        match outcome {
            RuleOutcome::Denied => Err(zuno_error::ToolError::Denied {
                tool: tool.to_owned(),
            }),
            _ if self.authorization.is_allow_all() => Ok(()),
            _ if self.requires_manual(&ask) => self.prompt_manual(origin, tool, ask).await,
            RuleOutcome::Permitted => Ok(()),
            RuleOutcome::Pending(pending) => self.prompt(origin, tool, ask, pending).await,
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
        "shell" => strings_at(args, &["command"]),
        "read" | "write" | "edit" => strings_at(args, &["filePath", "file_path", "path"]),
        "apply_patch" => patch_paths(args),
        "glob" | "grep" => strings_at(args, &["pattern", "query"]),
        "webfetch" => strings_at(args, &["url"]),
        "web_search" => strings_at(args, &["queries"]),
        "task" => strings_at(args, &["subagent_type", "subagentType"]),
        "skill" => strings_at(args, &["name"]),
        "notes" => strings_at(args, &["name", "prefix"]),
        "read_mcp_resource" => strings_at(args, &["uri", "resource_name", "server"]),
        "list_mcp_resources" | "list_mcp_resource_templates" => strings_at(args, &["server"]),
        "plan_get" | "plan_update" | "todo_get" | "todo_update" | "question" | "invalid"
        | "plan_exit" | "history" | "lsp" | "execute" => {
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
        if let zuno_error::ToolError::MutationConflict { conflict, .. } = error {
            let presentation = MutationConflictPresentation::from_conflict(conflict);
            let metadata = serde_json::to_value(&presentation)
                .expect("mutation conflict presentation is JSON-serializable");
            let output = ToolOutput::text(format!("{tool} conflict"), message)
                .with_metadata(METADATA_MUTATION_CONFLICT_KEY, metadata)
                .with_presentation(ToolResultPresentation::MutationConflict(presentation));
            return ToolDispatchResult::blocked(output, ToolBlockKind::Conflict);
        }
        let blocked = match error {
            zuno_error::ToolError::Denied { .. } => Some(ToolBlockKind::Denied),
            zuno_error::ToolError::InvalidArgs { .. } => Some(ToolBlockKind::InvalidArguments),
            zuno_error::ToolError::MutationConflict { .. } => Some(ToolBlockKind::Conflict),
            zuno_error::ToolError::NotFound { .. } => Some(ToolBlockKind::Unavailable),
            zuno_error::ToolError::Timeout { .. }
            | zuno_error::ToolError::NetworkTimeout { .. }
            | zuno_error::ToolError::Transient { .. }
            | zuno_error::ToolError::Failed { .. }
            | zuno_error::ToolError::Uncertain { .. } => None,
        };
        if let Some(kind) = blocked {
            return blocked_result(tool, message, kind);
        }
        if let zuno_error::ToolError::Uncertain { applied_paths, .. } = error {
            // A call can lose its outcome without having observed which paths it
            // touched — a supervisor that dies around a shell command, say. Pointing
            // the model at "the listed paths" when the list is empty tells it to
            // inspect nothing, so the empty case names the state to go looking for
            // instead.
            if applied_paths.is_empty() {
                message.push_str(
                    "\n\nRecovery: this call may have changed authoritative state before losing its final outcome, and it did not observe which. Inspect the state this call would have changed and continue from what is actually there; do not replay the call mechanically.",
                );
            } else {
                message.push_str(
                    "\n\nRecovery: this call changed authoritative state before losing its final outcome. Inspect the listed paths and continue from what is actually on disk; do not replay the call mechanically.",
                );
            }
            let presentation = UncertainMutationPresentation::new(applied_paths.clone());
            let mut output = ToolOutput::text(format!("{tool} uncertain"), message)
                .with_metadata("outcome", "uncertain")
                .with_metadata("uncertain", true)
                .with_presentation(ToolResultPresentation::UncertainMutation(presentation));
            for path in applied_paths {
                output = output.with_written_path(Path::new(path));
            }
            return ToolDispatchResult::error(output).with_uncertain_outcome(UncertainOutcome {
                tool: tool.to_owned(),
                applied_paths: applied_paths.clone(),
                cause: zuno_error::UncertainCause::LostOutcome,
            });
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

/// The dispatch result of a call a hard turn interruption stopped.
///
/// A cooperative return is not automatically a certain one. The tool may have settled a
/// decided outcome, or it may have been stopped between starting a side effect and
/// observing it, and only the tool can tell those apart — so when its settled output
/// carries [`CANCELLATION_METADATA_KEY`] saying the outcome is undecided, the recorded
/// interruption says `uncertain` too and the settled report the model reads gains the
/// sentence that asks for authoritative state to be inspected first. A tool that
/// attaches nothing keeps the previous reading, text included, and
/// [`ToolInterruption::Forced`] stays exactly as uncertain as it was: the grace period
/// elapsed, so nothing was observed at all.
fn interrupted_result(
    tool: &str,
    interruption: ToolInterruption,
    settled: Option<ToolDispatchResult>,
) -> ToolDispatchResult {
    let settled_is_uncertain = settled
        .as_ref()
        .is_some_and(|settled| cancellation_is_uncertain(&settled.output));
    let (forced, uncertain, message) = match interruption {
        ToolInterruption::Cooperative if settled_is_uncertain => (
            false,
            true,
            format!(
                "Tool `{tool}` acknowledged cancellation, but it was stopped before its work reached a decided outcome. Its final side-effect state is uncertain; inspect authoritative state before retrying.",
            ),
        ),
        ToolInterruption::Cooperative => (
            false,
            false,
            format!(
                "Tool `{tool}` acknowledged cancellation and completed its cleanup before returning."
            ),
        ),
        ToolInterruption::Forced => (
            true,
            true,
            format!(
                "Tool `{tool}` did not stop within {} seconds and was force-aborted. Its final side-effect state is uncertain; inspect authoritative state before retrying.",
                TOOL_INTERRUPT_SETTLE_GRACE.as_secs()
            ),
        ),
    };
    let mut output = match settled {
        // A settled call's own report is what the model reads, and only the tool can
        // write it: it names the output the call produced and the effects the tool
        // observed. What the tool cannot say is how the call ended, so an undecided
        // cancellation appends the dispatcher's sentence instead of dropping it —
        // recording `uncertain` in metadata alone would leave the model reading a
        // half-finished report with no instruction to check real state.
        Some(settled) => {
            let mut output = settled.output;
            if uncertain {
                let report = output.output.trim_end().to_owned();
                output.output = if report.is_empty() {
                    message
                } else {
                    format!("{report}\n\n{message}")
                };
            }
            output
        }
        None => ToolOutput::text(format!("{tool} interrupted"), message),
    };
    output.metadata.insert(
        INTERRUPTION_METADATA_KEY.to_owned(),
        json!({
            "mode": interruption.as_str(),
            "forced": forced,
            "uncertain": uncertain,
            "graceMs": TOOL_INTERRUPT_SETTLE_GRACE.as_millis(),
        }),
    );
    let result = ToolDispatchResult::interrupted(output, interruption);
    if !uncertain {
        return result;
    }
    // The paths come from the settled report rather than from the interruption,
    // because only the tool observed them: a cooperative return carries whatever it
    // had already written, and a forced abort carries nothing because the grace
    // period elapsed before the tool could say. An empty list is therefore a real
    // answer here and is recorded as one.
    let applied_paths = result
        .output
        .written_paths()
        .into_iter()
        .map(str::to_owned)
        .collect();
    result.with_uncertain_outcome(UncertainOutcome {
        tool: tool.to_owned(),
        applied_paths,
        cause: zuno_error::UncertainCause::Interrupted,
    })
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
            permission_patterns("shell", &serde_json::json!({"command": "git push"})),
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
            closest_tool_names("ToolSerch", &["shell", "tool_search", "web_search", "todo"]);
        assert_eq!(suggestions.first().map(String::as_str), Some("tool_search"));
        assert!(!suggestions.contains(&"shell".to_owned()));
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
            tool: "shell".to_owned(),
            elapsed: Duration::from_secs(120),
        };

        let result = tool_error_result("shell", ToolReplayPolicy::Never, &error);

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

    #[test]
    fn mutation_conflicts_are_typed_blocked_results_and_never_retryable() {
        let error = zuno_error::ToolError::MutationConflict {
            tool: "apply_patch".to_owned(),
            conflict: Box::new(zuno_error::ToolMutationConflict {
                kind: zuno_error::ToolMutationConflictKind::ContextMismatch,
                resource: "src/lib.rs".to_owned(),
                operation_digest: "patch-digest".to_owned(),
                observed_digest: Some("file-digest".to_owned()),
                hunk_index: Some(2),
                hunk_header: Some("impl Demo".to_owned()),
            }),
            source: Box::new(std::io::Error::other("hunk context was not found")),
        };

        let result = tool_error_result("apply_patch", ToolReplayPolicy::Never, &error);

        assert_eq!(result.blocked, Some(ToolBlockKind::Conflict));
        assert_eq!(result.recovery, None);
        assert_eq!(
            result.output.metadata[METADATA_MUTATION_CONFLICT_KEY]["kind"],
            "context_mismatch"
        );
        assert_eq!(
            result.output.metadata[METADATA_MUTATION_CONFLICT_KEY]["requiredAction"],
            "reread_and_revise"
        );
        assert!(matches!(
            result.output.presentation,
            Some(ToolResultPresentation::MutationConflict(_))
        ));
    }

    #[test]
    fn uncertain_mutations_preserve_observed_paths_and_typed_outcome() {
        let error = zuno_error::ToolError::Uncertain {
            tool: "apply_patch".to_owned(),
            applied_paths: vec!["/workspace/src/lib.rs".to_owned()],
            source: Box::new(std::io::Error::other("formatter response was lost")),
        };

        let result = tool_error_result("apply_patch", ToolReplayPolicy::Never, &error);

        assert!(result.is_error);
        assert_eq!(result.blocked, None);
        assert_eq!(result.recovery, None);
        // The typed disposition and `recovery` are separate fields for a reason: this
        // call must be inspected, and `recovery` is the field that says a call may be
        // issued again. Holding one must never be readable as holding the other.
        assert_eq!(
            result.uncertain,
            Some(UncertainOutcome {
                tool: "apply_patch".to_owned(),
                applied_paths: vec!["/workspace/src/lib.rs".to_owned()],
                cause: zuno_error::UncertainCause::LostOutcome,
            })
        );
        assert_eq!(result.output.metadata["outcome"], "uncertain");
        assert_eq!(result.output.metadata["uncertain"], true);
        assert_eq!(result.output.written_paths(), vec!["/workspace/src/lib.rs"]);
        assert!(matches!(
            result.output.presentation,
            Some(ToolResultPresentation::UncertainMutation(_))
        ));
        assert!(
            result
                .output
                .output
                .contains("do not replay the call mechanically")
        );
        assert!(
            result.output.output.contains("Inspect the listed paths"),
            "a call that observed its paths must point at them: {}",
            result.output.output
        );
    }

    #[test]
    fn an_uncertain_outcome_with_no_observed_paths_still_names_what_to_inspect() {
        // The shell tool reaches this: when the child-process guard's own machinery
        // fails, whether the command ran is unknown and so is what it touched.
        let error = zuno_error::ToolError::Uncertain {
            tool: "shell".to_owned(),
            applied_paths: Vec::new(),
            source: Box::new(std::io::Error::other("the child-process guard failed")),
        };

        let result = tool_error_result("shell", ToolReplayPolicy::Never, &error);

        assert!(result.is_error);
        assert_eq!(
            result.recovery, None,
            "an uncertain outcome is never replayed"
        );
        assert_eq!(
            result.uncertain,
            Some(UncertainOutcome {
                tool: "shell".to_owned(),
                applied_paths: Vec::new(),
                cause: zuno_error::UncertainCause::LostOutcome,
            }),
            "observing no paths is a real answer, not an absent disposition"
        );
        assert_eq!(result.output.metadata["uncertain"], true);
        assert!(result.output.written_paths().is_empty());
        assert!(
            !result.output.output.contains("listed paths"),
            "there is no list to inspect: {}",
            result.output.output
        );
        assert!(
            result
                .output
                .output
                .contains("Inspect the state this call would have changed"),
            "{}",
            result.output.output
        );
        assert!(
            result
                .output
                .output
                .contains("do not replay the call mechanically")
        );
    }

    /// A settled tool result, as the cooperative arm hands one to `interrupted_result`.
    fn settled_with(metadata: Value) -> ToolDispatchResult {
        let mut output = ToolOutput::text("shell", "partial progress\n");
        if let Value::Object(entries) = metadata {
            for (key, value) in entries {
                output.metadata.insert(key, value);
            }
        }
        ToolDispatchResult::success(output)
    }

    fn interruption_facts(result: &ToolDispatchResult) -> &Value {
        result
            .output
            .metadata
            .get("interruption")
            .expect("an interrupted result records how it was interrupted")
    }

    /// A tool that says its cancelled outcome is undecided is recorded as uncertain.
    ///
    /// Cooperative return used to mean certain, unconditionally: a `shell` command
    /// killed between starting a write and observing it was reported as having
    /// "completed its cleanup". The tool is the only layer that knows which of the two
    /// cancellations happened, so the dispatcher reads its claim instead of assuming.
    #[test]
    fn a_cooperative_cancellation_of_an_undecided_call_is_recorded_as_uncertain() {
        let result = interrupted_result(
            "shell",
            ToolInterruption::Cooperative,
            Some(settled_with(serde_json::json!({
                "cancellation": {
                    "cancelled": true,
                    "authoritative": false,
                    "uncertain": true,
                }
            }))),
        );

        let facts = interruption_facts(&result);
        assert_eq!(facts["mode"], "cooperative");
        assert_eq!(facts["uncertain"], true);
        assert_eq!(
            facts["forced"], false,
            "an uncertain outcome is not the same claim as a forced abort"
        );
        assert_eq!(result.interruption, Some(ToolInterruption::Cooperative));
        assert_eq!(
            result.uncertain,
            Some(UncertainOutcome {
                tool: "shell".to_owned(),
                applied_paths: Vec::new(),
                cause: zuno_error::UncertainCause::Interrupted,
            }),
            "the same obligation an undecided outcome carries, in the same typed field"
        );
        assert!(
            result.output.output.starts_with("partial progress"),
            "the settled output the tool preserved is what the model reads first: {}",
            result.output.output
        );
        assert!(
            result
                .output
                .output
                .contains("inspect authoritative state before retrying"),
            "an undecided outcome the model can only see in metadata is not reported: {}",
            result.output.output
        );
    }

    /// A tool that makes no cancellation claim keeps the reading it always had.
    ///
    /// Turning every cooperative cancellation uncertain would send the model to inspect
    /// authoritative state after a read-only call that stopped cleanly.
    #[test]
    fn a_cooperative_cancellation_without_a_claim_is_not_uncertain() {
        for settled in [
            settled_with(Value::Null),
            settled_with(serde_json::json!({
                "cancellation": { "cancelled": true, "uncertain": false }
            })),
            // A malformed claim is absent evidence, not a claim of uncertainty.
            settled_with(serde_json::json!({ "cancellation": "yes" })),
            settled_with(serde_json::json!({ "cancellation": { "uncertain": "yes" } })),
        ] {
            let result = interrupted_result("read", ToolInterruption::Cooperative, Some(settled));
            assert_eq!(
                interruption_facts(&result)["uncertain"],
                false,
                "{:?}",
                result.output.metadata
            );
            assert_eq!(
                result.output.output, "partial progress\n",
                "a cancellation nobody called undecided keeps the tool's report verbatim"
            );
            assert_eq!(
                result.uncertain, None,
                "a read-only call that stopped cleanly owes nobody an inspection"
            );
        }
    }

    /// A forced abort stays exactly as uncertain as it was.
    #[test]
    fn a_forced_interruption_is_uncertain_with_no_settled_output_to_read() {
        let result = interrupted_result("shell", ToolInterruption::Forced, None);

        let facts = interruption_facts(&result);
        assert_eq!(facts["mode"], "forced");
        assert_eq!(facts["forced"], true);
        assert_eq!(facts["uncertain"], true);
        assert_eq!(
            result.uncertain,
            Some(UncertainOutcome {
                tool: "shell".to_owned(),
                applied_paths: Vec::new(),
                cause: zuno_error::UncertainCause::Interrupted,
            })
        );
        assert!(
            result
                .output
                .output
                .contains("inspect authoritative state before retrying"),
            "{}",
            result.output.output
        );
    }

    /// An undecided cancellation asks for state inspection in the text, not only in
    /// metadata.
    ///
    /// A tool can declare its outcome undecided and still hand back a report that reads
    /// like ordinary progress. The model is answered with that text, so the demand for
    /// authoritative-state inspection has to be in it — whether or not the tool left
    /// anything of its own to keep.
    #[test]
    fn an_undecided_cancellation_message_asks_for_authoritative_state() {
        let mut silent = settled_with(serde_json::json!({
            "cancellation": { "uncertain": true }
        }));
        silent.output.output = String::new();
        let result = interrupted_result("shell", ToolInterruption::Cooperative, Some(silent));
        assert_eq!(interruption_facts(&result)["uncertain"], true);
        assert!(
            result
                .output
                .output
                .contains("inspect authoritative state before retrying"),
            "a tool that settled no text of its own must still be answered: {}",
            result.output.output
        );
        assert!(
            !result.output.output.contains("completed its cleanup"),
            "an undecided outcome must not be described as finished cleanup: {}",
            result.output.output
        );

        // Nothing settled at all is a different claim: no tool said anything, so the
        // reading a cooperative return has always had is what is recorded.
        let empty = interrupted_result("shell", ToolInterruption::Cooperative, None);
        assert_eq!(interruption_facts(&empty)["uncertain"], false);
        assert!(
            empty
                .output
                .output
                .contains("completed its cleanup before returning"),
            "a tool that settled nothing has made no claim: {}",
            empty.output.output
        );
    }

    /// The runtime reads back exactly the verdict the dispatcher recorded.
    ///
    /// This is the read the turn loop performs before it publishes
    /// `TurnEvent::ToolDispatchInterrupted`, so a live client presents the certainty
    /// stored on the tool result instead of re-deriving one from the interruption mode.
    #[test]
    fn a_recorded_interruption_verdict_is_readable_by_the_runtime() {
        let undecided = interrupted_result(
            "shell",
            ToolInterruption::Cooperative,
            Some(settled_with(serde_json::json!({
                "cancellation": { "uncertain": true }
            }))),
        );
        assert_eq!(recorded_interruption_uncertainty(&undecided), Some(true));

        let decided = interrupted_result(
            "shell",
            ToolInterruption::Cooperative,
            Some(settled_with(Value::Null)),
        );
        assert_eq!(recorded_interruption_uncertainty(&decided), Some(false));

        let forced = interrupted_result("shell", ToolInterruption::Forced, None);
        assert_eq!(recorded_interruption_uncertainty(&forced), Some(true));
    }

    /// A result carrying no verdict answers `None` so the caller falls back to the mode.
    ///
    /// The fallback is what keeps a result produced before this record existed readable:
    /// its mode is the reading it was written under.
    #[test]
    fn a_result_without_a_recorded_verdict_leaves_the_reading_to_its_caller() {
        assert_eq!(
            recorded_interruption_uncertainty(&ToolDispatchResult::success(ToolOutput::text(
                "shell", "done"
            ))),
            None
        );

        let mut malformed = ToolDispatchResult::interrupted(
            ToolOutput::text("shell", "partial").with_metadata(
                INTERRUPTION_METADATA_KEY,
                serde_json::json!({ "mode": "cooperative", "uncertain": "yes" }),
            ),
            ToolInterruption::Cooperative,
        );
        assert_eq!(recorded_interruption_uncertainty(&malformed), None);

        malformed.output.metadata.insert(
            INTERRUPTION_METADATA_KEY.to_owned(),
            Value::String("cooperative".to_owned()),
        );
        assert_eq!(recorded_interruption_uncertainty(&malformed), None);
    }

    /// The metadata key is a cross-crate contract, so its spelling is pinned here.
    ///
    /// The producer is `zuno_tools::shell::METADATA_CANCELLATION_KEY`, which this crate
    /// cannot import: dispatch is the layer those tools are handed to. Both sides pin
    /// the same string so a rename fails a test instead of silently disconnecting the
    /// claim from the reader.
    #[test]
    fn the_cancellation_metadata_key_is_the_one_the_shell_tool_writes() {
        assert_eq!(CANCELLATION_METADATA_KEY, "cancellation");
        assert!(cancellation_is_uncertain(
            &ToolOutput::text("shell", "").with_metadata(
                CANCELLATION_METADATA_KEY,
                serde_json::json!({ "uncertain": true })
            )
        ));
    }
}
