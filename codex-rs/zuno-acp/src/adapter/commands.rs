//! Permission presets (ACP session modes) and slash commands.
//!
//! The shapes follow the reference `codex-acp` adapter so a client such as Zed
//! sees the same surface from Zuno: ACP *modes* are approval/sandbox presets,
//! the collaboration mode (default/plan) is a config option toggled by `/plan`,
//! and the commands advertised through `available_commands_update` are the
//! subset of the Codex CLI slash commands that make sense without a terminal.

use super::*;

/// One ACP session mode: an approval policy, a reviewer and a sandbox preset.
pub(super) struct PermissionPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub approval_policy: &'static str,
    pub approvals_reviewer: &'static str,
    pub sandbox_type: &'static str,
}

pub(super) const PERMISSION_PRESETS: &[PermissionPreset] = &[
    PermissionPreset {
        id: "read-only",
        name: "Read-only",
        description: "Requires approval to edit files and access the internet.",
        approval_policy: "on-request",
        approvals_reviewer: "user",
        sandbox_type: "readOnly",
    },
    PermissionPreset {
        id: "workspace-write",
        name: "Workspace access",
        description: "Edit workspace files; ask before writing outside the workspace or accessing the network.",
        approval_policy: "on-request",
        approvals_reviewer: "user",
        sandbox_type: "workspaceWrite",
    },
    PermissionPreset {
        id: "agent",
        name: "Auto review",
        description: "Only ask for actions detected as potentially unsafe.",
        approval_policy: "on-request",
        approvals_reviewer: "auto_review",
        sandbox_type: "workspaceWrite",
    },
    // Mirrors examples/zuno-config/server-strict.config.toml: every command and
    // edit is approved first and then runs unsandboxed, because the standalone
    // server binary ships no sandbox helper (see docs/zuno-server-strict.md).
    PermissionPreset {
        id: "strict",
        name: "Strict",
        description: "Zuno server mode: approve every command and edit first; approved actions run without a sandbox.",
        approval_policy: "untrusted",
        approvals_reviewer: "user",
        sandbox_type: "dangerFullAccess",
    },
    PermissionPreset {
        id: "agent-full-access",
        name: "Full access",
        description: "Unrestricted access to the internet and any file on your computer.",
        approval_policy: "never",
        approvals_reviewer: "user",
        sandbox_type: "dangerFullAccess",
    },
];

/// The mode id shown when the thread's settings match no preset (for example a
/// granular approval policy or an external sandbox configured in config.toml).
pub(super) const CUSTOM_PERMISSION_MODE: &str = "custom";

pub(super) fn permission_preset(id: &str) -> Option<&'static PermissionPreset> {
    PERMISSION_PRESETS.iter().find(|preset| preset.id == id)
}

/// The preset the session currently matches, or [`CUSTOM_PERMISSION_MODE`].
pub(super) fn permission_mode_id(route: &SessionRoute) -> &'static str {
    PERMISSION_PRESETS
        .iter()
        .find(|preset| {
            preset.approval_policy == route.approval_policy
                && preset.approvals_reviewer == route.approvals_reviewer
                && preset.sandbox_type == route.sandbox_type
        })
        .map_or(CUSTOM_PERMISSION_MODE, |preset| preset.id)
}

/// The `sandboxPolicy` value `thread/settings/update` expects for a preset.
pub(super) fn sandbox_policy_json(sandbox_type: &str) -> Value {
    match sandbox_type {
        "readOnly" => json!({ "type": "readOnly", "networkAccess": false }),
        "dangerFullAccess" => json!({ "type": "dangerFullAccess" }),
        _ => json!({
            "type": "workspaceWrite",
            "writableRoots": [],
            "networkAccess": false,
            "excludeTmpdirEnvVar": false,
            "excludeSlashTmp": false,
        }),
    }
}

/// ACP `modes` for a session: every preset, plus the `custom` entry whenever the
/// thread started with settings that match none of them (it stays selectable
/// after a preset was applied, restoring those settings).
pub(super) fn session_modes(route: &SessionRoute) -> Value {
    let current = permission_mode_id(route);
    let mut available: Vec<Value> = PERMISSION_PRESETS
        .iter()
        .map(|preset| {
            json!({
                "id": preset.id,
                "name": preset.name,
                "description": preset.description,
                "_meta": { "zuno": {
                    "approvalPolicy": preset.approval_policy,
                    "approvalsReviewer": preset.approvals_reviewer,
                    "sandbox": preset.sandbox_type,
                } },
            })
        })
        .collect();
    if current == CUSTOM_PERMISSION_MODE || route.custom_permissions.is_some() {
        let description = match &route.custom_permissions {
            Some(custom) => format!(
                "approval {} reviewed by {}, sandbox {} (from config)",
                approval_policy_id(custom.get("approvalPolicy")),
                custom
                    .get("approvalsReviewer")
                    .and_then(Value::as_str)
                    .unwrap_or("user"),
                custom
                    .pointer("/sandboxPolicy/type")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
            ),
            None => format!(
                "approval {} reviewed by {}, sandbox {}",
                route.approval_policy, route.approvals_reviewer, route.sandbox_type
            ),
        };
        available.insert(
            0,
            json!({
                "id": CUSTOM_PERMISSION_MODE,
                "name": "Custom (from config)",
                "description": description,
            }),
        );
    }
    json!({ "currentModeId": current, "availableModes": available })
}

pub(super) const COLLABORATION_MODES: &[(&str, &str, &str)] = &[
    ("default", "Default", "Codex edits and runs as it works"),
    (
        "plan",
        "Plan",
        "Plan before making changes; asks questions instead of editing",
    ),
];

/// A `/name rest` prompt, when the first prompt block is a text block that
/// starts with `/`. `$skill` invocations are not commands: they stay prompts.
pub(super) fn parse_slash_command(prompt: Option<&Value>) -> Option<(String, String)> {
    let first = prompt?.as_array()?.first()?;
    if first.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    let text = first.get("text")?.as_str()?.trim();
    let rest = text.strip_prefix('/')?.trim_start();
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_end];
    if name.is_empty() {
        return None;
    }
    Some((
        name.to_ascii_lowercase(),
        rest[name_end..].trim().to_owned(),
    ))
}

/// Built-in commands, in the order the client shows them.
pub(super) const BUILTIN_COMMANDS: &[(&str, &str, Option<&str>)] = &[
    ("plan", "Toggle plan mode for the following turns.", None),
    (
        "compact",
        "Summarize the conversation to avoid hitting the context limit.",
        None,
    ),
    (
        "review",
        "Review uncommitted changes, or review with custom instructions.",
        Some("optional review instructions"),
    ),
    (
        "review-branch",
        "Review changes relative to a base branch.",
        Some("branch name"),
    ),
    (
        "review-commit",
        "Review a specific commit.",
        Some("commit sha"),
    ),
    (
        "status",
        "Display session configuration, account and token usage.",
        None,
    ),
    ("skills", "List available skills.", None),
    (
        "mcp",
        "List configured Model Context Protocol (MCP) servers.",
        None,
    ),
    (
        "goal",
        "Set a goal to keep pursuing.",
        Some("<objective> | clear | pause | resume"),
    ),
    ("rename", "Rename the current session.", Some("new name")),
    ("logout", "Sign out of the Codex account.", None),
];

/// The `available_commands_update` payload: built-ins plus one `$skill` entry
/// per discovered skill (a `$skill` prompt is forwarded to the model as is).
pub(super) fn available_commands_update(skills: &[(String, String)]) -> Value {
    let mut commands: Vec<Value> = BUILTIN_COMMANDS
        .iter()
        .map(|(name, description, hint)| {
            json!({
                "name": name,
                "description": description,
                "input": hint.map(|hint| json!({ "hint": hint })),
            })
        })
        .collect();
    for (name, description) in skills {
        let command = format!("${name}");
        if commands.iter().any(|existing| existing["name"] == command) {
            continue;
        }
        commands.push(json!({ "name": command, "description": description, "input": null }));
    }
    json!({ "sessionUpdate": "available_commands_update", "availableCommands": commands })
}

/// Skills as `(name, description)` from a `skills/list` response.
pub(super) fn skills_from_list(response: &Value) -> Vec<(String, String)> {
    response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("skills").and_then(Value::as_array))
        .flatten()
        .filter(|skill| {
            skill
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        })
        .filter_map(|skill| {
            let name = skill.get("name")?.as_str()?.to_owned();
            let description = skill
                .get("shortDescription")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .or_else(|| skill.get("description").and_then(Value::as_str))
                .unwrap_or(&name)
                .to_owned();
            Some((name, description))
        })
        .collect()
}

fn agent_text(text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text },
    })
}

/// What `session/prompt` does with a slash command.
pub(super) enum SlashOutcome {
    /// The command completed; this is the `session/prompt` response.
    Handled(Value),
    /// Run a normal turn with this App Server input instead of the prompt.
    Prompt(Vec<Value>),
}

impl CodexAcpAgent {
    /// Push the command list for a session after the lifecycle response.
    pub(super) async fn publish_available_commands(
        &self,
        session_id: &str,
        cwd: &str,
        client: &ClientConnection,
    ) {
        let skills = match self
            .app_request(
                "skills/list",
                json!({ "cwds": [cwd], "forceReload": false }),
            )
            .await
        {
            Ok(response) => skills_from_list(&response),
            Err(error) => {
                tracing::debug!(message = %error.message, "skills/list failed; advertising built-in commands only");
                Vec::new()
            }
        };
        if let Err(error) =
            client.session_update_after_response(session_id, available_commands_update(&skills))
        {
            tracing::warn!(message = %error.message, "could not schedule available_commands_update");
        }
    }

    /// Handle a `/command` prompt. `Ok(None)` means the prompt was not a
    /// recognised command and must run as an ordinary turn.
    pub(super) async fn handle_slash_command(
        &self,
        session_id: &str,
        route: &SessionRoute,
        client: &ClientConnection,
        params: &Value,
        message_id: Option<String>,
    ) -> Result<Option<SlashOutcome>, RpcError> {
        let Some((name, rest)) = parse_slash_command(params.get("prompt")) else {
            return Ok(None);
        };
        let done = |message_id: Option<String>| {
            Ok(Some(SlashOutcome::Handled(prompt_response(
                "end_turn", message_id,
            ))))
        };
        let say = |text: String| async move {
            client.session_update(session_id, agent_text(&text)).await
        };
        match name.as_str() {
            "plan" => {
                if !rest.is_empty() {
                    say("Command \"/plan\" takes no arguments.".to_owned()).await?;
                    return done(message_id);
                }
                let next = if route.collaboration_mode == "plan" {
                    "default"
                } else {
                    "plan"
                };
                let options = self
                    .set_option(&json!({
                        "sessionId": session_id, "configId": "collaboration_mode", "value": next,
                    }))
                    .await?;
                // The bridge changed a config option itself, so tell the client
                // (a `session/set_config_option` response would have carried it).
                client
                    .session_update(
                        session_id,
                        json!({
                            "sessionUpdate": "config_option_update",
                            "configOptions": options["configOptions"],
                        }),
                    )
                    .await?;
                say(format!(
                    "Plan mode {}.",
                    if next == "plan" {
                        "on: Zuno plans and asks before making changes"
                    } else {
                        "off"
                    }
                ))
                .await?;
                done(message_id)
            }
            "compact" => {
                let waiter = self.state.wait_for_thread_turn(session_id);
                self.app_request("thread/compact/start", json!({ "threadId": session_id }))
                    .await?;
                let outcome = waiter.await;
                match outcome.status.as_str() {
                    "completed" => done(message_id),
                    "interrupted" => Ok(Some(SlashOutcome::Handled(prompt_response(
                        "cancelled",
                        message_id,
                    )))),
                    _ => Err(RpcError::internal(
                        outcome
                            .error
                            .unwrap_or_else(|| "compaction failed".to_owned()),
                    )),
                }
            }
            "review" | "review-branch" | "review-commit" => {
                let target = match name.as_str() {
                    "review" if rest.is_empty() => json!({ "type": "uncommittedChanges" }),
                    "review" => json!({ "type": "custom", "instructions": rest }),
                    "review-branch" if rest.is_empty() => {
                        say("Command \"/review-branch\" requires a branch name.".to_owned())
                            .await?;
                        return done(message_id);
                    }
                    "review-branch" => json!({ "type": "baseBranch", "branch": rest }),
                    _ if rest.is_empty() => {
                        say("Command \"/review-commit\" requires a commit sha.".to_owned()).await?;
                        return done(message_id);
                    }
                    _ => json!({ "type": "commit", "sha": rest, "title": null }),
                };
                let response = self
                    .app_request(
                        "review/start",
                        json!({ "threadId": session_id, "target": target, "delivery": "inline" }),
                    )
                    .await?;
                let turn_id = response
                    .pointer("/turn/id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::internal("review/start response omitted turn.id"))?
                    .to_owned();
                self.mark_turn_active(session_id, &turn_id, client);
                let outcome = self.state.wait_for_turn(turn_id).await;
                match outcome.status.as_str() {
                    "completed" => done(message_id),
                    "interrupted" => Ok(Some(SlashOutcome::Handled(prompt_response(
                        "cancelled",
                        message_id,
                    )))),
                    _ => Err(RpcError::internal(
                        outcome.error.unwrap_or_else(|| "review failed".to_owned()),
                    )),
                }
            }
            "status" => {
                let text = self.status_text(session_id, route).await;
                say(text).await?;
                done(message_id)
            }
            "skills" => {
                let response = self
                    .app_request(
                        "skills/list",
                        json!({ "cwds": [route.cwd], "forceReload": false }),
                    )
                    .await?;
                let skills = skills_from_list(&response);
                let text = if skills.is_empty() {
                    "No skills configured.".to_owned()
                } else {
                    std::iter::once("Available skills:".to_owned())
                        .chain(
                            skills
                                .iter()
                                .map(|(name, description)| format!("- ${name}: {description}")),
                        )
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                say(text).await?;
                done(message_id)
            }
            "mcp" => {
                let response = self.app_request("mcpServerStatus/list", json!({})).await?;
                let servers: Vec<String> = response
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|server| {
                        format!(
                            "- {}: {} tools, {} resources, auth={}",
                            server.get("name").and_then(Value::as_str).unwrap_or("?"),
                            server
                                .get("tools")
                                .and_then(Value::as_object)
                                .map_or(0, serde_json::Map::len),
                            server
                                .get("resources")
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len),
                            server
                                .get("authStatus")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown"),
                        )
                    })
                    .collect();
                let text = if servers.is_empty() {
                    "No MCP servers configured.".to_owned()
                } else {
                    std::iter::once("Configured MCP servers:".to_owned())
                        .chain(servers)
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                say(text).await?;
                done(message_id)
            }
            "goal" => match rest.as_str() {
                "" => {
                    say(
                        "Command \"/goal\" requires <objective>, clear, pause or resume."
                            .to_owned(),
                    )
                    .await?;
                    done(message_id)
                }
                "clear" => {
                    self.app_request("thread/goal/clear", json!({ "threadId": session_id }))
                        .await?;
                    say("Goal cleared.".to_owned()).await?;
                    done(message_id)
                }
                "pause" => {
                    self.app_request(
                        "thread/goal/set",
                        json!({ "threadId": session_id, "status": "paused" }),
                    )
                    .await?;
                    say("Goal paused.".to_owned()).await?;
                    done(message_id)
                }
                "resume" => {
                    self.app_request(
                        "thread/goal/set",
                        json!({ "threadId": session_id, "status": "active" }),
                    )
                    .await?;
                    Ok(Some(SlashOutcome::Prompt(vec![json!({
                        "type": "text",
                        "text": "Continue working toward the active goal.",
                        "text_elements": [],
                    })])))
                }
                objective if objective.chars().count() > 4000 => {
                    say(
                        "Command \"/goal\" requires goal text of at most 4000 characters."
                            .to_owned(),
                    )
                    .await?;
                    done(message_id)
                }
                objective => {
                    self.app_request(
                        "thread/goal/set",
                        json!({ "threadId": session_id, "objective": objective, "status": "active" }),
                    )
                    .await?;
                    Ok(Some(SlashOutcome::Prompt(vec![json!({
                        "type": "text",
                        "text": "Continue working toward the active goal.",
                        "text_elements": [],
                    })])))
                }
            },
            "rename" => {
                if rest.is_empty() {
                    say("Command \"/rename\" requires a new name.".to_owned()).await?;
                    return done(message_id);
                }
                self.app_request(
                    "thread/name/set",
                    json!({ "threadId": session_id, "name": rest }),
                )
                .await?;
                say(format!("Session renamed to \"{rest}\".")).await?;
                done(message_id)
            }
            "logout" => {
                self.app_request("account/logout", json!({})).await?;
                say("Logged out of the Codex account.".to_owned()).await?;
                done(message_id)
            }
            // Unknown commands (and `$skill` prompts, which never reach here)
            // are ordinary prompts for the model.
            _ => Ok(None),
        }
    }

    /// Record a turn started by a command so steering and cancellation see it.
    pub(super) fn mark_turn_active(
        &self,
        session_id: &str,
        turn_id: &str,
        client: &ClientConnection,
    ) {
        let mut sessions = lock(&self.state.sessions);
        if let Some(current) = sessions.get_mut(session_id) {
            current.active_turn_id = Some(turn_id.to_owned());
            current.client = client.session_scoped();
        }
    }

    async fn status_text(&self, session_id: &str, route: &SessionRoute) -> String {
        let account = match self.app_request("account/read", json!({})).await {
            Ok(response) => describe_account(&response),
            Err(_) => "unknown".to_owned(),
        };
        let mut lines = vec![
            format!("**Model:** {} ({})", route.model, route.model_provider),
            format!(
                "**Reasoning effort:** {}",
                route.effort.as_deref().unwrap_or("default")
            ),
            format!("**Collaboration mode:** {}", route.collaboration_mode),
            format!(
                "**Permissions:** {} (approval {}, reviewer {}, sandbox {})",
                permission_mode_id(route),
                route.approval_policy,
                route.approvals_reviewer,
                route.sandbox_type
            ),
            format!("**Directory:** {}", route.cwd),
            format!("**Account:** {account}"),
            format!("**Session:** `{session_id}`"),
        ];
        if let Ok(limits) = self.app_request("account/rateLimits/read", json!({})).await {
            lines.extend(describe_rate_limits(&limits));
        }
        lines.join("  \n")
    }
}

fn describe_account(response: &Value) -> String {
    let account = response.get("account").unwrap_or(response);
    match account.get("type").and_then(Value::as_str) {
        Some("apiKey") => "API key configured".to_owned(),
        Some("chatgpt") => format!(
            "ChatGPT {} ({})",
            account
                .get("planType")
                .and_then(Value::as_str)
                .unwrap_or("plan"),
            account
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or("unknown email"),
        ),
        Some("amazonBedrock") => "Amazon Bedrock".to_owned(),
        Some(other) => other.to_owned(),
        None => "not logged in".to_owned(),
    }
}

fn describe_rate_limits(response: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let limits = response.get("rateLimits").unwrap_or(response);
    for (label, key) in [
        ("Primary limit", "primary"),
        ("Secondary limit", "secondary"),
    ] {
        if let Some(window) = limits.get(key)
            && let Some(used) = window.get("usedPercent").and_then(Value::as_f64)
        {
            let minutes = window.get("windowDurationMins").and_then(Value::as_u64);
            let window_label = match minutes {
                Some(m) if m >= 1440 => format!("{}d", m / 1440),
                Some(m) if m >= 60 => format!("{}h", m / 60),
                Some(m) => format!("{m}m"),
                None => String::new(),
            };
            lines.push(format!(
                "**{label}{}:** {:.0}% left",
                if window_label.is_empty() {
                    String::new()
                } else {
                    format!(" ({window_label})")
                },
                (100.0 - used).max(0.0)
            ));
        }
    }
    lines
}
