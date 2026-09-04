//! The read-only catalogue operations: `agent`, `command`, `skill`, `reference`.
//!
//! # The envelope is not decoration
//!
//! Every catalogue and filesystem operation except `fs/read` answers
//! `{location, data}` (`packages/server/src/location.ts:15-27`). A client that
//! reads `response.data` against a bare array gets `undefined`, so emitting the
//! array unwrapped would break every SDK caller while still looking like a
//! working endpoint in a smoke test. [`LocationEnvelope`] exists so no handler in
//! this module can forget it.
//!
//! # Where the data comes from
//!
//! Nothing here is new: `zuno-catalog` already resolves agents, commands, skills
//! and references for the CLI, and `zuno-config` already discovers the config tree.
//! These handlers project those resolved values into Zuno's HTTP response types,
//! rather than maintaining a second resolver.

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Map, Value};
use zuno_catalog::agent::{self, AgentMode};
use zuno_catalog::command;
use zuno_catalog::reference::{ReferenceTarget, ResolvedReferences};
use zuno_catalog::skill::discovery::SkillOptions;
use zuno_config::schema::Config;
use zuno_config::schema::permission::PermissionAction;
use zuno_permission::rules_from_config;

use super::blocking::Budget;
use super::error::ApiError;
use super::state::ApiState;

/// The HTTP system prompts.
///
/// # Why these bytes live here rather than in `zuno-catalog`
///
/// These assets are owned by the HTTP projection because that surface may evolve
/// independently from the CLI catalogue.
mod v2 {
    /// Explore-agent prompt.
    pub const PROMPT_EXPLORE: &str = include_str!("v2/agent-explore.txt");
    /// Compaction-agent prompt.
    pub const PROMPT_COMPACTION: &str = include_str!("v2/agent-compaction.txt");
    /// Title-agent prompt.
    pub const PROMPT_TITLE: &str = include_str!("v2/agent-title.txt");
    /// Summary-agent prompt.
    pub const PROMPT_SUMMARY: &str = include_str!("v2/agent-summary.txt");
}

/// The V2 system prompt for a native agent, when V2 declares one.
///
/// `build` gets [`V2_BUILD_SYSTEM`] through `item.system ??=`, so a user prompt wins
/// there; the other four are assigned unconditionally (`plugin/agent.ts:166-201`),
/// so V2's copy wins over anything the v1 files carry.
fn v2_system(name: &str) -> Option<&'static str> {
    match name {
        "explore" => Some(v2::PROMPT_EXPLORE),
        "compaction" => Some(
            v2::PROMPT_COMPACTION
                .strip_suffix('\n')
                .unwrap_or(v2::PROMPT_COMPACTION),
        ),
        "title" => Some(v2::PROMPT_TITLE),
        "summary" => Some(v2::PROMPT_SUMMARY),
        _ => None,
    }
}

/// The `build` agent's system prompt on the V2 surface.
///
/// `packages/core/src/plugin/agent.ts:11-13`, applied at `:127` as
/// `item.system ??= BUILD_SYSTEM`. This is the **second** v1/v2 seam in this port:
/// `zuno-catalog`'s `build` correctly has no prompt, because the v1 agent module it
/// mirrors has none, and `opencode debug agent` prints the v1 shape. The V2 HTTP
/// surface adds this one. Reporting the v1 absence here would answer `system:
/// undefined` for the default agent, which is what a client renders as "this agent
/// has no instructions".
const V2_BUILD_SYSTEM: &str = "You are an AI coding agent. Help the user accomplish software engineering tasks by inspecting the workspace, making targeted changes, and using tools according to the configured permissions.";

/// The V2 native roster in its declaration order.
///
/// `packages/core/src/plugin/agent.ts:124-204` calls `draft.update` in exactly this
/// order, and the map it writes into preserves insertion order, so this is the
/// order `/api/agent` answers in. `zuno-catalog` returns its own (sorted) order
/// because `agent list` sorts for display, so the two disagree and the roster has
/// to be re-ordered here rather than trusted as-is.
const V2_NATIVE_ORDER: &[&str] = &[
    "build",
    "plan",
    "general",
    "explore",
    "compaction",
    "title",
    "summary",
];

/// Upstream's `{location, data}` success envelope.
///
/// `workspaceID` is omitted rather than null when absent, which is what the
/// oracle emits: the field is `Schema.optional` and JSON drops an `undefined`
/// value (`packages/schema/src/location.ts:14-20`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocationEnvelope<T> {
    /// Which directory, workspace and project the answer was computed for.
    pub location: LocationBody,
    /// The operation's payload.
    pub data: T,
}

/// The `location` object every catalogue response carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocationBody {
    /// The session directory.
    pub directory: String,
    /// The workspace, when one is selected.
    #[serde(rename = "workspaceID", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The project the directory resolves to.
    pub project: ProjectBody,
}

/// The `location.project` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectBody {
    /// The project identifier, `global` outside a repository.
    pub id: String,
    /// The project's root directory.
    pub directory: String,
}

impl<T: Serialize> IntoResponse for LocationEnvelope<T> {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

/// An envelope whose `data` may be null.
///
/// The schema is `Schema.UndefinedOr(...)`
/// (`protocol/src/groups/integration.ts:25-30`), which reads as "the key may be
/// omitted", and that is what I predicted from the source. **A live probe of
/// 1.18.12 disagreed**: `GET /api/integration/definitely-absent` answers
/// `{"location":{…},"data":null}`, with the key present and null. Upstream's Effect
/// encoder emits the declared property rather than dropping it. The measurement
/// wins over the reading, so `data` is serialised as `null`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OptionalEnvelope<T> {
    /// Which directory, workspace and project the answer was computed for.
    pub location: LocationBody,
    /// The payload, `null` when the operation resolved nothing.
    pub data: Option<T>,
}

impl<T: Serialize> IntoResponse for OptionalEnvelope<T> {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

/// Upstream's `Agent.Info` (`packages/schema/src/agent.ts:19-31`), in its field
/// order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentInfo {
    /// The agent's name.
    pub id: String,
    /// The agent's pinned model, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Per-agent request overrides.
    pub request: RequestBody,
    /// The system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// When to use the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where the agent may be selected.
    pub mode: &'static str,
    /// Hidden from pickers.
    pub hidden: bool,
    /// Display colour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Iteration cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    /// The resolved permission ruleset.
    pub permissions: Vec<PermissionRule>,
}

/// A `provider/model` reference, split the way upstream splits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelRef {
    /// The model id.
    pub id: String,
    /// The provider id.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// The variant, when pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// Upstream's `Provider.Request` — headers and body overlays.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestBody {
    /// Header overlay.
    pub headers: Map<String, Value>,
    /// Body overlay.
    pub body: Map<String, Value>,
    /// The variant this request selects, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl RequestBody {
    /// An empty overlay, which is what a subject with no provider options carries.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            headers: Map::new(),
            body: Map::new(),
            variant: None,
        }
    }
}

/// One entry of an agent's permission ruleset, in upstream's spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PermissionRule {
    /// The permission being decided.
    pub action: String,
    /// The pattern it applies to.
    pub resource: String,
    /// `allow`, `ask` or `deny`.
    pub effect: &'static str,
}

/// Upstream's `Command.Info` (`packages/schema/src/command.ts:7-15`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandInfo {
    /// The name it is invoked by.
    pub name: String,
    /// The prompt text.
    pub template: String,
    /// What it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The agent to run as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The model to run with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Whether it runs in a subtask.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtask: Option<bool>,
}

/// Upstream's `Skill.Info` (`packages/schema/src/skill.ts:19-26`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillInfo {
    /// The skill name.
    pub name: String,
    /// The frontmatter description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether it is offered as a slash command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash: Option<bool>,
    /// Where it was loaded from.
    pub location: String,
}

/// Upstream's `Reference.Info` (`packages/schema/src/reference.ts:33-39`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReferenceInfo {
    /// The key it was declared under.
    pub name: String,
    /// The materialised directory.
    pub path: String,
    /// Human description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hidden from pickers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// Where it points.
    pub source: ReferenceSource,
}

/// The `source` union on a reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ReferenceSource {
    /// A local directory.
    Local {
        /// Always `local`.
        #[serde(rename = "type")]
        kind: &'static str,
        /// The directory.
        path: String,
        /// Human description.
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Hidden from pickers.
        #[serde(skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
    },
    /// A git repository.
    Git {
        /// Always `git`.
        #[serde(rename = "type")]
        kind: &'static str,
        /// The repository.
        repository: String,
        /// The pinned branch.
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        /// Human description.
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Hidden from pickers.
        #[serde(skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
    },
}

/// The directory, worktree and discovered config one request is answered from.
pub(super) struct Resolution {
    /// The session directory.
    pub(super) directory: PathBuf,
    /// The repository root, when the directory is inside one.
    pub(super) worktree: Option<PathBuf>,
    /// The merged config tree.
    pub(super) config: Config,
}

impl Resolution {
    /// Resolves the request context from the session directory and the injected
    /// environment.
    ///
    /// # Errors
    /// Returns [`ApiError::ConfigInvalid`] with the config loader's own report
    /// when the user's config tree does not parse, because answering with an
    /// empty catalogue in that case would tell the user their config is fine.
    pub(super) fn open(state: &ApiState) -> Result<Self, ApiError> {
        let directory = PathBuf::from(state.directory());
        let project = zuno_paths::project::resolve_project(&directory);
        let worktree = project.vcs.as_ref().map(|_| project.directory.clone());
        let config =
            zuno_config::discovery::discover_with(&zuno_config::discovery::DiscoveryOptions::new(
                &directory,
                worktree.as_deref(),
                state.env().clone(),
            ))
            .map_err(|error| ApiError::ConfigInvalid(error.report()))?;
        Ok(Self {
            directory,
            worktree,
            config,
        })
    }
}

/// `GET /api/agent` — the resolved agent roster.
///
/// # Errors
/// Returns [`ApiError::ConfigInvalid`] when the config tree does not parse and
/// [`ApiError::CatalogUnavailable`] when agent discovery fails on disk.
pub async fn agents(
    State(state): State<ApiState>,
) -> Result<LocationEnvelope<Vec<AgentInfo>>, ApiError> {
    let resolved = Resolution::open(&state)?;
    let agents = agent::load(
        &resolved.directory,
        resolved.worktree.as_deref(),
        state.env(),
    )
    .map_err(|error| ApiError::CatalogUnavailable(error.to_string()))?;
    let layout = zuno_paths::Layout::resolve(state.env());
    let worktree = resolved
        .worktree
        .clone()
        .unwrap_or_else(|| resolved.directory.clone());
    let mut data = agents
        .into_iter()
        .map(|entry| agent_info(entry, &resolved.config, &layout, &worktree))
        .collect::<Vec<_>>();
    data.sort_by_key(|entry| {
        V2_NATIVE_ORDER
            .iter()
            .position(|name| *name == entry.id)
            .unwrap_or(V2_NATIVE_ORDER.len())
    });
    Ok(state.envelope(data))
}

/// The V2 permission ruleset for one agent.
///
/// `PermissionV2.merge` is `rulesets.flat()`
/// (`packages/core/src/permission.ts:88-90`), so a ruleset is a concatenation and
/// **its order is the semantics**: resolution is find-last, which is how `explore`
/// can start from `{*: allow}` and still end up read-only.
///
/// This is the V2 set, deliberately not `zuno-cli`'s v1 set: V2 whitelists only the
/// tool-output and temp directories, where v1 also whitelists every discovered
/// skill and reference root. Answering with the v1 rules would advertise
/// permissions the V2 runtime does not grant.
fn v2_permissions(name: &str, layout: &zuno_paths::Layout, worktree: &Path) -> Vec<PermissionRule> {
    let plans = layout.data().join("plans");
    let readonly_external = vec![
        v2_rule("external_directory", "*", "ask"),
        v2_rule(
            "external_directory",
            &glob_of(&layout.tool_output()),
            "allow",
        ),
        v2_rule("external_directory", &glob_of(layout.temp()), "allow"),
    ];
    let mut rules = vec![v2_rule("*", "*", "allow")];
    rules.extend(readonly_external.iter().cloned());
    rules.extend([
        v2_rule("question", "*", "deny"),
        v2_rule("plan_enter", "*", "deny"),
        v2_rule("plan_exit", "*", "deny"),
        v2_rule("read", "*", "allow"),
        v2_rule("read", "*.env", "ask"),
        v2_rule("read", "*.env.*", "ask"),
        v2_rule("read", "*.env.example", "allow"),
    ]);
    match name {
        "build" => rules.extend([
            v2_rule("question", "*", "allow"),
            v2_rule("plan_enter", "*", "allow"),
        ]),
        "plan" => rules.extend([
            v2_rule("question", "*", "allow"),
            v2_rule("plan_exit", "*", "allow"),
            v2_rule("external_directory", &glob_of(&plans), "allow"),
            v2_rule("edit", "*", "deny"),
            v2_rule("edit", ".zuno/plans/*.md", "allow"),
            v2_rule(
                "edit",
                &relative_path(worktree, &plans.join("*.md")),
                "allow",
            ),
        ]),
        "general" => rules.extend([
            v2_rule("plan_update", "*", "deny"),
            v2_rule("todo_update", "*", "deny"),
        ]),
        "explore" => {
            rules.extend([
                v2_rule("*", "*", "deny"),
                v2_rule("grep", "*", "allow"),
                v2_rule("glob", "*", "allow"),
                v2_rule("webfetch", "*", "allow"),
                v2_rule("web_search", "*", "allow"),
                v2_rule("read", "*", "allow"),
            ]);
            rules.extend(readonly_external);
        }
        "compaction" | "title" | "summary" | "council-synth" => {
            rules.push(v2_rule("*", "*", "deny"));
        }
        _ => {}
    }
    rules
}

fn v2_rule(action: &str, resource: &str, effect: &'static str) -> PermissionRule {
    PermissionRule {
        action: action.to_owned(),
        resource: resource.to_owned(),
        effect,
    }
}

fn glob_of(path: &Path) -> String {
    path.join("*").to_string_lossy().into_owned()
}

/// `path.relative(from, to)`, which upstream uses for the plan-edit rule so a
/// worktree outside the data directory still gets a usable pattern.
fn relative_path(from: &Path, to: &Path) -> String {
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    let shared = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in shared..from.len() {
        relative.push("..");
    }
    for component in &to[shared..] {
        relative.push(component.as_os_str());
    }
    relative.to_string_lossy().into_owned()
}

/// Projects one resolved agent onto upstream's `Agent.Info`.
fn agent_info(
    entry: agent::Agent,
    config: &Config,
    layout: &zuno_paths::Layout,
    worktree: &Path,
) -> AgentInfo {
    let mut permissions = if entry.source.is_native() {
        v2_permissions(&entry.name, layout, worktree)
    } else {
        Vec::new()
    };
    if let Some(user) = &config.permission {
        permissions.extend(rules_from_config(user).into_iter().map(permission_rule));
    }
    if let Some(agent_rules) = &entry.permission {
        permissions.extend(
            rules_from_config(agent_rules)
                .into_iter()
                .map(permission_rule),
        );
    }
    let is_v2_build = entry.source.is_native() && entry.name == "build";
    let v2_native_system = entry
        .source
        .is_native()
        .then(|| v2_system(&entry.name))
        .flatten();
    AgentInfo {
        id: entry.name,
        model: entry
            .model
            .as_deref()
            .map(|value| model_ref(value, entry.variant.clone())),
        request: RequestBody {
            headers: Map::new(),
            body: entry.options.clone(),
            variant: None,
        },
        system: v2_native_system
            .map(ToOwned::to_owned)
            .or(entry.prompt)
            .or_else(|| is_v2_build.then(|| V2_BUILD_SYSTEM.to_owned())),
        description: entry.description,
        mode: match entry.mode {
            AgentMode::Subagent => "subagent",
            AgentMode::Primary => "primary",
            AgentMode::All => "all",
        },
        hidden: entry.hidden.unwrap_or(false),
        color: entry
            .color
            .as_ref()
            .map(|color| serde_json::to_value(color).ok())
            .and_then(|value| match value {
                Some(Value::String(text)) => Some(text),
                _ => None,
            }),
        steps: entry.steps.map(std::num::NonZeroU32::get),
        permissions,
    }
}

/// Maps this port's permission rule onto upstream's `{action, resource, effect}`.
fn permission_rule(rule: zuno_permission::Rule) -> PermissionRule {
    PermissionRule {
        action: rule.permission,
        resource: rule.pattern,
        effect: match rule.action {
            PermissionAction::Allow => "allow",
            PermissionAction::Ask => "ask",
            PermissionAction::Deny => "deny",
        },
    }
}

/// Splits a `provider/model[:variant]` string the way upstream's `Model.parse`
/// does: the first `/` separates provider from model, and everything after it
/// stays in the model id.
fn model_ref(value: &str, variant: Option<String>) -> ModelRef {
    let (provider_id, id) = value.split_once('/').map_or_else(
        || (String::new(), value.to_owned()),
        |(left, right)| (left.to_owned(), right.to_owned()),
    );
    ModelRef {
        id,
        provider_id,
        variant,
    }
}

/// `GET /api/command` — the three built-ins plus the user's config commands.
///
/// # V2 has three levels, not four
///
/// Skills and commands are separate catalog entries. The command roster contains
/// Zuno's built-ins plus configured command files; discovered skills are available
/// through `/api/skill` and are not promoted into a client's command palette.
///
/// The built-in templates substitute the **project** directory, which is what
/// `plugin/command.ts:15` does through `location.project.directory`; the session
/// directory is a different value and substituting it would name the wrong root in
/// the prompt the model reads.
///
/// # Errors
/// Returns [`ApiError::ConfigInvalid`] when the config tree does not parse.
pub async fn commands(
    State(state): State<ApiState>,
) -> Result<LocationEnvelope<Vec<CommandInfo>>, ApiError> {
    let resolved = Resolution::open(&state)?;
    let project_directory = state.project_directory().to_owned();
    let configured = command::load_map(
        &resolved.directory,
        resolved.worktree.as_deref(),
        state.env(),
    )
    .map_err(|error| ApiError::ConfigInvalid(error.report()))?;
    let sources = command::Sources::new(&project_directory).with_config(Some(&configured));
    let registry = command::Registry::build(&sources);
    let data = registry
        .list()
        .map(|info| CommandInfo {
            name: info.name.clone(),
            template: match &info.template {
                command::Template::Text(text) => text.clone(),
                // An MCP template is one round trip away from being text and
                // this operation is read-only, so it is reported by reference
                // rather than fetched inside a GET.
                command::Template::Mcp(template) => {
                    format!("{}/{}", template.client, template.prompt)
                }
            },
            description: info.description.clone(),
            agent: info.agent.clone(),
            model: info.model.as_deref().map(|value| model_ref(value, None)),
            subtask: info.subtask,
        })
        .collect();
    Ok(state.envelope(data))
}

/// Runs skill discovery off the request thread, inside the catalogue budget.
///
/// `zuno_catalog::skill::load` walks the disk and may fetch remote skill indexes, so
/// its future is not `Send` and axum cannot hold it across an await. Moving it onto
/// a blocking thread with its own current-thread runtime is what the CLI already
/// does with `block_on`, and it keeps a slow disk walk off the reactor besides.
///
/// It is charged to [`Budget::Catalog`] for the reason
/// [`crate::api::blocking`] gives: this is a sibling of the filesystem and maintenance
/// handlers on the same unbounded blocking pool, its duration is bounded by a remote
/// index rather than by local disk, and bounding those two while leaving this one
/// unbounded would only move the starvation to `GET /api/skill`.
///
/// # Errors
/// Returns [`ApiError::CatalogUnavailable`] when the discovery thread cannot be
/// started or panics.
async fn load_skills(options: SkillOptions) -> Result<zuno_catalog::skill::Skills, ApiError> {
    super::blocking::run(Budget::Catalog, move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ApiError::CatalogUnavailable(error.to_string()))?;
        Ok(runtime.block_on(zuno_catalog::skill::load(&options)))
    })
    .await
}

/// `GET /api/skill` — discovered skill metadata.
///
/// # Errors
/// Returns [`ApiError::ConfigInvalid`] when the config tree does not parse.
pub async fn skills(
    State(state): State<ApiState>,
) -> Result<LocationEnvelope<Vec<SkillInfo>>, ApiError> {
    let resolved = Resolution::open(&state)?;
    let options = SkillOptions::from_config(
        &resolved.directory,
        resolved.worktree.as_deref(),
        state.env(),
        &resolved.config,
    );
    let loaded = load_skills(options).await?;
    let project_directory = state.project_directory().to_owned();
    let configured = command::load_map(
        &resolved.directory,
        resolved.worktree.as_deref(),
        state.env(),
    )
    .map_err(|error| ApiError::ConfigInvalid(error.report()))?;
    let command_sources = command::Sources::new(&project_directory).with_config(Some(&configured));
    let command_registry = command::Registry::build(&command_sources);
    let slash_sources = loaded
        .slash_invokable(command_registry.list().map(|command| command.name.as_str()))
        .into_iter()
        .map(|skill| skill.location.clone())
        .collect::<std::collections::HashSet<_>>();
    let data = loaded
        .sorted()
        .into_iter()
        .map(|skill| {
            let builtin = zuno_catalog::skill::builtin::is_location(&skill.location);
            let slash = slash_sources.contains(&skill.location).then_some(true);
            SkillInfo {
                description: skill.description,
                location: if builtin {
                    format!("/builtin/{}.md", skill.name)
                } else {
                    skill.location
                },
                name: skill.name,
                slash,
            }
        })
        .collect();
    Ok(state.envelope(data))
}

/// `GET /api/reference` — the declared reference roots.
///
/// # Errors
/// Returns [`ApiError::ConfigInvalid`] when the config tree does not parse.
pub async fn references(
    State(state): State<ApiState>,
) -> Result<LocationEnvelope<Vec<ReferenceInfo>>, ApiError> {
    let resolved = Resolution::open(&state)?;
    let declared = resolved.config.references.as_ref();
    let data = ResolvedReferences::resolve(declared)
        .iter()
        .map(|reference| reference_info(reference, &resolved.directory))
        .collect();
    Ok(state.envelope(data))
}

/// Projects one resolved reference onto upstream's `Reference.Info`.
fn reference_info(
    reference: &zuno_catalog::reference::ResolvedReference,
    directory: &Path,
) -> ReferenceInfo {
    let hidden = reference.hidden.then_some(true);
    let (path, source) = match &reference.target {
        ReferenceTarget::Local { path } | ReferenceTarget::Shorthand(path) => {
            let absolute = absolute_reference(directory, path);
            (
                absolute.clone(),
                ReferenceSource::Local {
                    kind: "local",
                    path: absolute,
                    description: reference.description.clone(),
                    hidden,
                },
            )
        }
        ReferenceTarget::Git { repository, branch } => (
            zuno_paths::global()
                .repos()
                .join(zuno_paths::sha1::hex(repository.as_bytes()))
                .to_string_lossy()
                .into_owned(),
            ReferenceSource::Git {
                kind: "git",
                repository: repository.clone(),
                branch: branch.clone(),
                description: reference.description.clone(),
                hidden,
            },
        ),
    };
    ReferenceInfo {
        name: reference.name.clone(),
        path,
        description: reference.description.clone(),
        hidden,
        source,
    }
}

/// Resolves a declared reference path against the session directory.
fn absolute_reference(directory: &Path, path: &str) -> String {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate.to_string_lossy().into_owned()
    } else {
        directory.join(candidate).to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_reference_splits_on_the_first_separator_only() {
        let reference = model_ref("anyapi/openai/gpt-5.4", None);
        assert_eq!(reference.provider_id, "anyapi");
        assert_eq!(reference.id, "openai/gpt-5.4");
    }

    #[test]
    fn absent_optional_data_is_a_present_null_as_the_oracle_emits_it() {
        let envelope = OptionalEnvelope::<u8> {
            location: LocationBody {
                directory: "/repo".to_owned(),
                workspace_id: None,
                project: ProjectBody {
                    id: "global".to_owned(),
                    directory: "/".to_owned(),
                },
            },
            data: None,
        };
        let encoded = serde_json::to_value(&envelope).expect("envelope serializes");
        assert_eq!(
            encoded["data"],
            serde_json::Value::Null,
            "1.18.12 answers `data: null` for an unknown integration, measured live"
        );
        assert!(encoded["location"].get("workspaceID").is_none());
    }
}
