//! Tool visibility: which tools are hidden from the model entirely.
//!
//! A tool that can never be invoked must not be advertised. Shipping its schema
//! costs prompt tokens on every request and invites a recovery spiral where the
//! model calls it, is refused, and then reasons about the refusal.
//!
//! Hiding and refusing are separate code paths. This module only hides tools
//! whose *last* matching permission rule is an unconditional deny; a narrower
//! deny (`{"bash": {"rm *": "deny"}}`) leaves the tool visible and is enforced
//! at call time by [`crate::PermissionEngine::authorize`].
//!
//! Oracle: `packages/opencode/src/permission/index.ts:204-219`
//! (`disabled` and `visibleTools`).

use crate::types::Rule;
use crate::wildcard::wildcard_match;
use oc_config::schema::permission::PermissionAction;
use std::collections::BTreeSet;

/// Tools routed through the `edit` permission key.
///
/// Oracle: `const edits = ["edit", "write", "apply_patch"]`.
pub const EDIT_TOOLS: [&str; 3] = ["edit", "write", "apply_patch"];

/// Tools routed through the `read` permission key.
///
/// Oracle: `const reads = ["list_mcp_resources", "list_mcp_resource_templates",
/// "read_mcp_resource"]`. These are the three MCP resource tools declared in
/// `packages/opencode/src/session/tools.ts:28-32` as `MCP_RESOURCE_TOOLS`.
pub const READ_TOOLS: [&str; 3] = [
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
];

/// Map a tool name onto the permission key that governs it.
///
/// Every tool not in an alias group is governed by its own name.
#[must_use]
pub fn permission_key(tool: &str) -> &str {
    if EDIT_TOOLS.contains(&tool) {
        return "edit";
    }
    if READ_TOOLS.contains(&tool) {
        return "read";
    }
    tool
}

/// Layer rulesets in order; later rules win under the `findLast` evaluator.
///
/// Oracle: `merge(...rulesets) => rulesets.flat()`.
#[must_use]
pub fn merge_rulesets(rulesets: &[&[Rule]]) -> Vec<Rule> {
    rulesets.concat()
}

/// Merge agent rules with session rules the way the runtime does.
///
/// The oracle calls `Permission.merge(agent.permission, session.permission ?? [])`
/// in both `session/tools.ts:82` and `tool/registry.ts:280`, so **session rules
/// are appended last and therefore win** under `findLast`. Agent rules are the
/// base layer, not the override layer.
#[must_use]
pub fn merge_agent_session(agent: &[Rule], session: &[Rule]) -> Vec<Rule> {
    merge_rulesets(&[agent, session])
}

/// Return whether a tool is advertised to the model.
///
/// The tool is hidden when the last rule whose permission key matches denies
/// every pattern. The key match reuses [`wildcard_match`], the same primitive
/// the evaluator uses, so a `{"*": "deny"}` outer key hides every tool.
#[must_use]
pub fn is_tool_visible(tool: &str, rules: &[Rule]) -> bool {
    !is_tool_hidden(tool, rules)
}

/// Return whether a tool is hidden from the model.
///
/// Inverse of [`is_tool_visible`].
#[must_use]
pub fn is_tool_hidden(tool: &str, rules: &[Rule]) -> bool {
    let key = permission_key(tool);
    rules
        .iter()
        .rev()
        .find(|rule| wildcard_match(key, &rule.permission))
        .is_some_and(|rule| rule.pattern == "*" && rule.action == PermissionAction::Deny)
}

/// Collect the names of every tool hidden from the model.
///
/// Oracle: `disabled(tools, ruleset)`.
#[must_use]
pub fn disabled_tools<'a, I>(tools: I, rules: &[Rule]) -> BTreeSet<String>
where
    I: IntoIterator<Item = &'a str>,
{
    tools
        .into_iter()
        .filter(|tool| is_tool_hidden(tool, rules))
        .map(str::to_owned)
        .collect()
}

/// Filter a tool list down to what the model may see, preserving order.
///
/// Oracle: `visibleTools(tools, ruleset)`. `id` extracts the tool name that the
/// permission keys are matched against.
#[must_use]
pub fn visible_tools<T, I, F>(tools: I, rules: &[Rule], id: F) -> Vec<T>
where
    I: IntoIterator<Item = T>,
    F: Fn(&T) -> &str,
{
    tools
        .into_iter()
        .filter(|tool| is_tool_visible(id(tool), rules))
        .collect()
}

/// Drop hidden tools from an existing list in place, preserving order.
pub fn retain_visible_tools<T, F>(tools: &mut Vec<T>, rules: &[Rule], id: F)
where
    F: Fn(&T) -> &str,
{
    tools.retain(|tool| is_tool_visible(id(tool), rules));
}
