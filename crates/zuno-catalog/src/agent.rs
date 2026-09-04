//! Zuno's native agents, `agents.<name>` config entries, and Markdown definitions
//! discovered under `{agent,agents}/**/*.md`.
//!
//! # Name rule
//!
//! An agent's name is its path relative to the config directory, minus the
//! `agent/` or `agents/` prefix and minus the extension. So
//! `agent/review/security.md` is the agent `review/security` — **not** `security`.
//!
//! A frontmatter `name:` key overrides the derived name entirely, because the
//! resolved map is keyed by the final name. `agent/original.md` with
//! `name: renamed-by-frontmatter` therefore exposes only
//! `renamed-by-frontmatter`.
//!
//! # Layer order
//!
//! Global Zuno config, explicit config, project files, and `.zuno` directories
//! are merged before Markdown agents. `ZUNO_CONFIG_CONTENT` is re-applied after
//! Markdown so the explicit environment layer remains authoritative.
//!
//! Every step keeps the author's key order. Frontmatter is parsed into
//! [`zuno_config::schema::ordered::OrderedJson`] and reaches [`AgentConfig`] as
//! JSON text, and layers merge through [`zuno_config::discovery::merge_layers`],
//! so a `permission.rules` block is evaluated in the order it was written rather
//! than in the alphabetical order a `serde_json::Map` would impose on it.
//!
//! # What is deliberately not here
//!
//! * `{mode,modes}/*.md` is not scanned. Zuno reads only `agent/` and `agents/`.
//! * `maxSteps` is not accepted. `tools` is a sequence of exact tool names;
//!   legacy boolean maps are rejected by [`AgentConfig`].
//! * The unknown-key sweep into `options` is not reimplemented.
//!   [`zuno_config::schema::agent::AgentConfig`]'s `Deserialize` performs it, and
//!   this module simply consumes the result.
//! * Permissions are not resolved into a ruleset. The `permission` key is carried
//!   verbatim and the built-in overlays are exposed as data; see
//!   [`builtin::Builtin::permission_overlay`].

pub mod builtin;
pub mod frontmatter;

use serde::Serialize;
use serde_json::Value;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use zuno_config::discovery::{DiscoveryOptions, discover_with, merge_layers};
use zuno_config::schema::agent::{AgentColor, AgentConfig, AgentReasoning};
use zuno_config::schema::ordered::{OrderedJson, OrderedMap};
use zuno_config::schema::permission::PermissionConfig;
use zuno_config::schema::{Config, JsonMap};
use zuno_error::ConfigError;
use zuno_paths::{Env, Layout};

pub use zuno_config::schema::agent::AgentMode;

/// Prefixes stripped from a Markdown agent's relative path.
///
/// Order matters: the first match wins.
pub const AGENT_DIRECTORY_PREFIXES: [&str; 2] = ["agent/", "agents/"];

/// The Markdown layout scanned in every Zuno config directory.
///
/// Reproduced here as documentation; the walk is implemented directly because
/// `{a,b}` brace expansion plus `**` plus dotfile inclusion plus symlink following
/// is clearer as two explicit roots than as a glob dependency.
pub const AGENT_GLOB: &str = "{agent,agents}/**/*.md";

/// Where an agent's definition came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSource {
    /// One of the thirteen native agents, with no user definition on top.
    Native,
    /// A native agent a user definition has modified.
    NativeOverridden,
    /// An `agents.<name>` entry in a config file or env layer.
    Config,
    /// A Markdown file, named by its path.
    Markdown {
        /// The file the definition was read from.
        path: PathBuf,
    },
}

impl AgentSource {
    /// Whether this is a native agent, overridden or not.
    ///
    /// `agent list` sorts natives before everything else, and an override does not
    /// change that classification.
    #[must_use]
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native | Self::NativeOverridden)
    }
}

/// A resolved agent, after built-ins, config entries, and Markdown definitions
/// have been folded together.
///
/// The catalog representation before runtime permissions are resolved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Agent {
    /// The agent's name, which is also how a user selects it.
    pub name: String,
    /// When to use the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where the agent may be used. A newly defined agent defaults to
    /// [`AgentMode::All`].
    pub mode: AgentMode,
    /// Hidden from the `@` autocomplete menu.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// Model in `provider/model` form, unparsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Model-specific variant, applied only with the agent's own model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Provider-neutral reasoning level, applied only with the agent's own model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<AgentReasoning>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-sampling cutoff, serialized as `topP`.
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Display colour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<AgentColor>,
    /// The system prompt. For a Markdown agent this is the trimmed body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Maximum agentic iterations before a text-only response is forced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<NonZeroU32>,
    /// Exact model-visible tool allowlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Exact child-agent allowlist for direct delegation and workflows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegates: Option<Vec<String>>,
    /// Skills loaded at the start of every turn for this agent.
    #[serde(rename = "requiredSkills", skip_serializing_if = "Option::is_none")]
    pub required_skills: Option<Vec<String>>,
    /// Provider options, including every unknown key swept in by
    /// [`AgentConfig`]'s deserializer.
    pub options: JsonMap,
    /// The `permission` key verbatim, unresolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionConfig>,
    /// Where the definition came from.
    pub source: AgentSource,
}

impl Agent {
    /// The `name (mode)` header `agent list` prints.
    #[must_use]
    pub fn header(&self) -> String {
        format!("{} ({})", self.name, mode_label(self.mode))
    }
}

/// The lowercase mode name the CLI prints and config accepts.
#[must_use]
pub fn mode_label(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Subagent => "subagent",
        AgentMode::Primary => "primary",
        AgentMode::All => "all",
    }
}

/// One Markdown agent definition as read from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct MarkdownAgent {
    /// The name, derived from the path or overridden by a frontmatter `name:`.
    pub name: String,
    /// The file it was read from.
    pub path: PathBuf,
    /// The frontmatter fields, with the body already installed as `prompt`.
    pub config: AgentConfig,
}

/// Which file each Markdown-defined agent was read from, keyed by agent name.
///
/// Carried alongside the agent map so provenance metadata does not participate in
/// the user-config deep merge.
pub type MarkdownOrigins = Vec<(String, PathBuf)>;

/// The merged agent map and the provenance of its Markdown-defined entries.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LoadedAgents {
    /// Every `agents.<name>` entry, after all layers have merged.
    pub agents: OrderedMap<AgentConfig>,
    /// Where each Markdown-defined agent came from.
    pub origins: MarkdownOrigins,
}

/// Derive an agent name from a path relative to one scanned config directory.
///
/// Prefix matching is anchored. When no prefix matches, the basename is used so
/// an unexpected path cannot retain unrelated parent segments.
#[must_use]
pub fn entry_name_from_path(relative: &str, prefixes: &[&str]) -> String {
    let normalized = relative.replace('\\', "/");
    let candidate = prefixes
        .iter()
        .find_map(|prefix| normalized.strip_prefix(prefix))
        .map_or_else(|| basename(&normalized).to_owned(), str::to_owned);
    strip_extension(&candidate)
}

/// `path.basename` over a `/`-separated string.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `path.extname` semantics: only a dot after at least one character in the final
/// segment counts, so `.hidden` keeps its whole name while `a.md` loses `.md`.
fn strip_extension(candidate: &str) -> String {
    let segment = basename(candidate);
    let Some(dot) = segment.rfind('.') else {
        return candidate.to_owned();
    };
    if dot == 0 {
        return candidate.to_owned();
    }
    let cut = candidate.len() - (segment.len() - dot);
    candidate[..cut].to_owned()
}

/// Read every Markdown agent under one config directory.
///
/// Scans `agent/` and `agents/`, recursively, including dot-directories and
/// following symlinks.
/// Files are visited in sorted order so a directory listing's arbitrary order
/// cannot decide which of two same-named definitions wins.
///
/// A file whose frontmatter cannot be parsed is skipped with a warning. A
/// document that parses but violates the Zuno agent schema is fatal.
///
/// # Errors
///
/// [`ConfigError::Io`] when a directory or file cannot be read, and
/// [`ConfigError::Invalid`] when a definition violates the agent schema.
pub fn discover_in_directory(dir: &Path) -> Result<Vec<MarkdownAgent>, ConfigError> {
    let mut found = Vec::new();
    for root_name in ["agent", "agents"] {
        let root = dir.join(root_name);
        if !root.is_dir() {
            continue;
        }
        for file in markdown_files(&root)? {
            let Some(agent) = read_markdown_agent(dir, &file)? else {
                continue;
            };
            found.push(agent);
        }
    }
    Ok(found)
}

/// Every `*.md` under `root`, recursively, sorted, following symlinks.
pub(crate) fn markdown_files(root: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let mut files = Vec::new();
    let mut queue = vec![root.to_path_buf()];
    let mut visited = Vec::new();
    while let Some(dir) = queue.pop() {
        // Following symlinks means a cycle is reachable; canonicalizing each
        // visited directory bounds the walk without refusing legitimate symlinked
        // agent trees.
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if visited.contains(&key) {
            continue;
        }
        visited.push(key);

        let entries = std::fs::read_dir(&dir).map_err(|source| ConfigError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut local = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| ConfigError::Io {
                path: dir.clone(),
                source,
            })?;
            local.push(entry.path());
        }
        local.sort();
        for path in local {
            if path.is_dir() {
                queue.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Parse one Markdown agent, returning `None` when its frontmatter is unreadable.
///
/// # Errors
///
/// [`ConfigError::Io`] when the file cannot be read, and [`ConfigError::Invalid`]
/// when the frontmatter does not satisfy the native agent schema.
pub fn read_markdown_agent(dir: &Path, file: &Path) -> Result<Option<MarkdownAgent>, ConfigError> {
    let text = std::fs::read_to_string(file).map_err(|source| ConfigError::Io {
        path: file.to_path_buf(),
        source,
    })?;

    // An unparseable head takes the file out of the list rather than failing the
    // whole load.
    let Ok(document) = frontmatter::parse(&text) else {
        tracing::warn!(
            path = %file.display(),
            "skipping agent: its frontmatter could not be parsed"
        );
        return Ok(None);
    };

    let relative = relative_path(dir, file);
    let derived = entry_name_from_path(&relative, &AGENT_DIRECTORY_PREFIXES);

    let mut object = document.data;
    // The derived name is a default a frontmatter `name:` may replace, while the
    // body always wins over a frontmatter `prompt:`.
    let name = object
        .get("name")
        .and_then(OrderedJson::as_str)
        .map_or(derived, str::to_owned);
    object.insert("name", OrderedJson::String(name.clone()));
    for key in ["options", "permission"] {
        if !object.contains_key(key) {
            object.insert(key, OrderedJson::Object(OrderedMap::new()));
        }
    }
    object.insert(
        "prompt",
        OrderedJson::String(document.content.trim().to_owned()),
    );

    let config: AgentConfig =
        from_ordered_json(&object).map_err(|source| ConfigError::Invalid {
            path: file.to_path_buf(),
            issues: vec![zuno_error::ConfigIssue::new(
                ["agent", name.as_str()],
                source.to_string(),
            )],
        })?;

    Ok(Some(MarkdownAgent {
        name,
        path: file.to_path_buf(),
        config,
    }))
}

/// Deserialize a frontmatter object through JSON **text**.
///
/// `serde_json::from_value` would rebuild the object as a `serde_json::Map` — a
/// `BTreeMap` in this workspace — and sort its keys on the way in, and
/// `permission.rules` precedence is the author's key order. JSON text is the one
/// carrier every `Deserialize` impl in the schema reads in order.
pub(crate) fn from_ordered_json<T: serde::de::DeserializeOwned>(
    object: &OrderedMap<OrderedJson>,
) -> Result<T, serde_json::Error> {
    serde_json::from_str(&serde_json::to_string(object)?)
}

/// `path.relative(dir, item)` with `/` separators, for the name rule.
pub(crate) fn relative_path(dir: &Path, file: &Path) -> String {
    file.strip_prefix(dir)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The Markdown agent layer across every config directory, in discovery order.
///
/// A later directory wins. [`zuno_paths::Layout::config_directories`] returns
/// directories in that precedence order.
///
/// # Errors
///
/// Propagates the first [`ConfigError`] from [`discover_in_directory`].
pub fn discover_markdown(dirs: &[PathBuf]) -> Result<Vec<MarkdownAgent>, ConfigError> {
    let mut found = Vec::new();
    for dir in dirs {
        found.extend(discover_in_directory(dir)?);
    }
    Ok(found)
}

/// Deep-merge one agent map over another.
///
/// The merge is [`zuno_config::discovery::merge_layers`] — the same
/// order-preserving deep merge config discovery applies between two `zuno.json`
/// layers, and the one [`crate::command::merge_command_maps`] already uses. Objects
/// merge key-by-key and anything else is replaced; a key both layers set keeps the
/// base layer's position and takes the overlay's value; a key only the overlay
/// sets is appended after the base block. Nothing sorts, so a `permission.rules`
/// block reaches the merged [`AgentConfig`] in the order its authors wrote it.
/// Merging through `serde_json::Value` alphabetized it, and rule precedence is
/// last-match-wins over that order.
///
/// # Errors
///
/// [`ConfigError`] when the merged map no longer satisfies the agent schema.
pub fn merge_agent_maps(
    base: &OrderedMap<AgentConfig>,
    overlay: &OrderedMap<AgentConfig>,
) -> Result<OrderedMap<AgentConfig>, ConfigError> {
    let merged = merge_layers([
        Config {
            agent: Some(base.clone()),
            ..Config::default()
        },
        Config {
            agent: Some(overlay.clone()),
            ..Config::default()
        },
    ])?;
    Ok(merged.agent.unwrap_or_default())
}

/// remeda's `mergeDeep`, for provider options only.
///
/// Options live in a [`JsonMap`], which is sorted by type, so no author key order
/// exists here to protect — unlike `permission.rules`, nothing downstream reads
/// provider options positionally.
fn merge_deep(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(existing) => merge_deep(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, source) => *target = source,
    }
}

/// Fold an agent map over the thirteen native agents.
///
/// * `disable: true` removes the agent, native or not.
/// * A name that is not native becomes a new agent with `mode: "all"`.
/// * An absent key leaves the native value in place.
/// * `options` deep-merges rather than replacing.
///
/// The returned order is the native agents first, then new agents in map
/// order. [`list`] applies the display sort separately.
#[must_use]
pub fn resolve(agents: &OrderedMap<AgentConfig>, origins: &[(String, PathBuf)]) -> Vec<Agent> {
    let mut resolved: Vec<Agent> = builtin::all().into_iter().map(from_builtin).collect();

    for (name, config) in agents.iter() {
        if config.disable == Some(true) {
            resolved.retain(|agent| agent.name != name);
            continue;
        }
        let position = resolved.iter().position(|agent| agent.name == name);
        let index = match position {
            Some(index) => index,
            None => {
                resolved.push(new_agent(name));
                resolved.len() - 1
            }
        };
        apply(&mut resolved[index], config);
        if let Some((_, path)) = origins.iter().find(|(key, _)| key == name) {
            resolved[index].source = match &resolved[index].source {
                source if source.is_native() => AgentSource::NativeOverridden,
                _ => AgentSource::Markdown { path: path.clone() },
            };
        }
    }
    resolved
}

fn from_builtin(builtin: builtin::Builtin) -> Agent {
    Agent {
        name: builtin.name.to_owned(),
        description: builtin.description.map(str::to_owned),
        mode: builtin.mode,
        hidden: builtin.hidden.then_some(true),
        model: None,
        variant: None,
        reasoning: None,
        temperature: builtin.temperature,
        top_p: None,
        color: None,
        prompt: builtin.prompt.map(str::to_owned),
        steps: None,
        tools: None,
        delegates: builtin
            .delegates
            .map(|targets| targets.iter().map(|target| (*target).to_owned()).collect()),
        required_skills: None,
        options: JsonMap::new(),
        permission: None,
        source: AgentSource::Native,
    }
}

/// A name with no native definition becomes a primary-and-subagent agent.
fn new_agent(name: &str) -> Agent {
    Agent {
        name: name.to_owned(),
        description: None,
        mode: AgentMode::All,
        hidden: None,
        model: None,
        variant: None,
        reasoning: None,
        temperature: None,
        top_p: None,
        color: None,
        prompt: None,
        steps: None,
        tools: None,
        delegates: None,
        required_skills: None,
        options: JsonMap::new(),
        permission: None,
        source: AgentSource::Config,
    }
}

/// Apply one user overlay while preserving absent native fields.
fn apply(agent: &mut Agent, config: &AgentConfig) {
    if agent.source == AgentSource::Native {
        agent.source = AgentSource::NativeOverridden;
    }
    if let Some(model) = &config.model {
        agent.model = Some(model.clone());
    }
    if let Some(variant) = &config.variant {
        agent.variant = Some(variant.clone());
        agent.reasoning = None;
    }
    if let Some(reasoning) = config.reasoning {
        agent.reasoning = Some(reasoning);
        agent.variant = None;
    }
    if let Some(prompt) = &config.prompt {
        agent.prompt = Some(prompt.clone());
    }
    if let Some(description) = &config.description {
        agent.description = Some(description.clone());
    }
    if let Some(temperature) = config.temperature {
        agent.temperature = Some(temperature);
    }
    if let Some(top_p) = config.top_p {
        agent.top_p = Some(top_p);
    }
    if let Some(mode) = config.mode {
        agent.mode = mode;
    }
    if let Some(color) = &config.color {
        agent.color = Some(color.clone());
    }
    if let Some(hidden) = config.hidden {
        agent.hidden = Some(hidden);
    }
    if let Some(steps) = config.steps {
        agent.steps = Some(steps);
    }
    if let Some(tools) = &config.tools {
        agent.tools = Some(tools.clone());
    }
    if let Some(delegates) = &config.delegates {
        agent.delegates = Some(delegates.clone());
    }
    if let Some(required_skills) = &config.required_skills {
        agent.required_skills = Some(required_skills.clone());
    }

    // A `name` key renames the resolved agent while the config lookup key remains
    // unchanged.
    if let Some(name) = config.extra.get("name").and_then(Value::as_str) {
        agent.name = name.to_owned();
    }

    if let Some(options) = &config.options {
        let mut merged = Value::Object(std::mem::take(&mut agent.options));
        merge_deep(&mut merged, Value::Object(options.clone()));
        if let Value::Object(map) = merged {
            agent.options = map;
        }
    }

    if let Some(permission) = &config.permission {
        agent.permission = Some(permission.clone());
    }
}

/// Every agent in Zuno CLI display order: natives first, then name order.
#[must_use]
pub fn list(agents: &OrderedMap<AgentConfig>, origins: &[(String, PathBuf)]) -> Vec<Agent> {
    let mut resolved = resolve(agents, origins);
    resolved.sort_by(|left, right| {
        left.source
            .is_native()
            .cmp(&right.source.is_native())
            .reverse()
            .then_with(|| locale_compare(&left.name, &right.name))
    });
    resolved
}

/// Case-insensitive primary ordering for agent names.
///
/// Agent names come from file paths and config keys, so the primary key is the
/// lowercase name and the raw name breaks ties deterministically.
fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

/// Discover and resolve every agent for one working directory.
///
/// Assembles merged Zuno config, Markdown definitions, then
/// `ZUNO_CONFIG_CONTENT` re-applied on top so the explicit environment layer
/// remains authoritative.
///
/// # Errors
///
/// Propagates [`ConfigError`] from config discovery, from reading an agent
/// directory, or from a Markdown definition that violates the agent schema.
pub fn load(
    directory: &Path,
    worktree: Option<&Path>,
    env: &Env,
) -> Result<Vec<Agent>, ConfigError> {
    let loaded = load_map(directory, worktree, env)?;
    Ok(list(&loaded.agents, &loaded.origins))
}

/// The merged agent map plus the Markdown provenance [`list`] needs.
///
/// Exposed so a caller that wants the map — the turn loop resolving one agent by
/// name — does not have to build and sort the whole list.
///
/// # Errors
///
/// As [`load`].
pub fn load_map(
    directory: &Path,
    worktree: Option<&Path>,
    env: &Env,
) -> Result<LoadedAgents, ConfigError> {
    let options = DiscoveryOptions::new(directory, worktree, env.clone());
    let config = discover_with(&options)?;
    let base = config.agent.clone().unwrap_or_default();

    let layout = Layout::resolve(env);
    let dirs = layout.config_directories(directory, worktree);
    let markdown = discover_markdown(&dirs)?;

    let mut overlay = OrderedMap::new();
    let mut origins: MarkdownOrigins = Vec::with_capacity(markdown.len());
    for agent in markdown {
        overlay.insert(agent.name.clone(), agent.config);
        origins.retain(|(name, _)| name != &agent.name);
        origins.push((agent.name, agent.path));
    }

    let mut agents = merge_agent_maps(&base, &overlay)?;
    if let Some(text) = env.truthy_value("ZUNO_CONFIG_CONTENT")
        && let Ok(layer) = serde_json::from_str::<Config>(text)
        && let Some(from_env) = layer.agent
    {
        agents = merge_agent_maps(&agents, &from_env)?;
        for (name, _) in from_env.iter() {
            origins.retain(|(key, _)| key != name);
        }
    }

    Ok(LoadedAgents { agents, origins })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_config::schema::permission::{PermissionAction, PermissionRule};

    fn map(json: serde_json::Value) -> OrderedMap<AgentConfig> {
        serde_json::from_value(json).expect("agent map fixture should deserialize")
    }

    /// Deserialize a fixture from JSON **text**.
    ///
    /// `serde_json::from_value` sorts an object's keys before the schema sees them,
    /// so it cannot express a `permission.rules` fixture at all: the order under
    /// test would already be gone in the input.
    fn map_text(json: &str) -> OrderedMap<AgentConfig> {
        serde_json::from_str(json).expect("agent map fixture should deserialize")
    }

    fn rule_keys(agents: &OrderedMap<AgentConfig>, name: &str) -> Vec<String> {
        agents
            .get(name)
            .and_then(|config| config.permission.as_ref())
            .expect("the merged agent keeps its policy")
            .rules
            .iter()
            .map(|(key, _)| key.to_owned())
            .collect()
    }

    fn names(agents: &[Agent]) -> Vec<&str> {
        agents.iter().map(|agent| agent.name.as_str()).collect()
    }

    #[test]
    fn the_name_is_the_relative_path_minus_prefix_and_extension() {
        assert_eq!(
            entry_name_from_path("agent/review/security.md", &AGENT_DIRECTORY_PREFIXES),
            "review/security"
        );
        assert_eq!(
            entry_name_from_path("agents/deep/nested/thing.md", &AGENT_DIRECTORY_PREFIXES),
            "deep/nested/thing"
        );
        assert_eq!(
            entry_name_from_path("agent/flat.md", &AGENT_DIRECTORY_PREFIXES),
            "flat"
        );
    }

    #[test]
    fn a_windows_separator_normalizes_before_the_prefix_match() {
        assert_eq!(
            entry_name_from_path("agent\\review\\security.md", &AGENT_DIRECTORY_PREFIXES),
            "review/security"
        );
    }

    #[test]
    fn a_path_with_no_prefix_falls_back_to_its_basename() {
        // The prefix is anchored, so a coincidental `agent` segment deeper in the
        // path does not key the agent.
        assert_eq!(
            entry_name_from_path("other/agent/security.md", &AGENT_DIRECTORY_PREFIXES),
            "security"
        );
    }

    #[test]
    fn only_the_final_extension_is_stripped_and_a_dotfile_keeps_its_name() {
        assert_eq!(
            entry_name_from_path("agent/a.tar.md", &AGENT_DIRECTORY_PREFIXES),
            "a.tar"
        );
        assert_eq!(
            entry_name_from_path("agent/.hidden", &AGENT_DIRECTORY_PREFIXES),
            ".hidden"
        );
        assert_eq!(
            entry_name_from_path("agent/dir/.hidden", &AGENT_DIRECTORY_PREFIXES),
            "dir/.hidden"
        );
    }

    #[test]
    fn the_first_matching_prefix_wins() {
        // `agent/` is checked before `agents/`, so a path under `agents/` whose
        // next segment is also `agent` is not double-stripped.
        assert_eq!(
            entry_name_from_path("agents/agent/x.md", &AGENT_DIRECTORY_PREFIXES),
            "agent/x"
        );
    }

    #[test]
    fn the_native_roster_exists_with_no_user_config_at_all() {
        let agents = resolve(&OrderedMap::new(), &[]);
        let expected = builtin::all()
            .into_iter()
            .map(|agent| agent.name)
            .collect::<Vec<_>>();
        assert_eq!(names(&agents), expected);
        assert!(
            agents
                .iter()
                .all(|agent| agent.source == AgentSource::Native)
        );
    }

    #[test]
    fn a_new_agent_defaults_to_mode_all() {
        let agents = resolve(&map(serde_json::json!({ "fresh": {} })), &[]);
        let fresh = agents
            .iter()
            .find(|agent| agent.name == "fresh")
            .expect("the new agent should exist");
        assert_eq!(fresh.mode, AgentMode::All);
        assert_eq!(fresh.source, AgentSource::Config);
    }

    #[test]
    fn native_orchestrator_carries_the_exact_delegate_allowlist() {
        let agents = resolve(&OrderedMap::new(), &[]);
        let orchestrator = agents
            .iter()
            .find(|agent| agent.name == "orchestrator")
            .expect("orchestrator exists");
        assert_eq!(
            orchestrator
                .delegates
                .as_ref()
                .map(|targets| { targets.iter().map(String::as_str).collect::<Vec<_>>() }),
            Some(vec![
                "deep",
                "fixer",
                "general",
                "explorer",
                "librarian",
                "oracle",
                "looker"
            ])
        );
        assert!(
            agents
                .iter()
                .filter(|agent| agent.name != "orchestrator")
                .all(|agent| agent.delegates.is_none())
        );
    }

    #[test]
    fn a_user_definition_overrides_a_built_in_and_it_stays_native() {
        let agents = resolve(
            &map(serde_json::json!({
                "plan": { "description": "overridden plan", "mode": "all" },
            })),
            &[],
        );
        let plan = agents
            .iter()
            .find(|agent| agent.name == "plan")
            .expect("plan should still exist");
        assert_eq!(plan.mode, AgentMode::All);
        assert_eq!(plan.description.as_deref(), Some("overridden plan"));
        assert_eq!(plan.source, AgentSource::NativeOverridden);
        assert!(
            plan.source.is_native(),
            "an override stays in the native block"
        );
    }

    #[test]
    fn an_override_that_omits_a_field_keeps_the_built_ins_value() {
        let agents = resolve(
            &map(serde_json::json!({ "title": { "model": "anthropic/haiku" } })),
            &[],
        );
        let title = agents
            .iter()
            .find(|agent| agent.name == "title")
            .expect("title exists");
        assert_eq!(title.model.as_deref(), Some("anthropic/haiku"));
        assert_eq!(
            title.temperature,
            Some(0.5),
            "the built-in temperature survives"
        );
        assert_eq!(title.prompt.as_deref(), Some(builtin::PROMPT_TITLE));
        assert_eq!(title.hidden, Some(true));
    }

    #[test]
    fn disable_removes_an_agent_entirely() {
        let agents = resolve(
            &map(serde_json::json!({ "plan": { "disable": true } })),
            &[],
        );
        assert!(!names(&agents).contains(&"plan"));
        assert_eq!(agents.len(), builtin::all().len() - 1);
    }

    #[test]
    fn unknown_frontmatter_keys_land_in_options_via_the_config_schema() {
        // The sweep itself belongs to zuno-config; this asserts it survives the fold.
        let agents = resolve(
            &map(serde_json::json!({
                "custom": { "reasoningEffort": "high", "thinking": { "type": "enabled" } },
            })),
            &[],
        );
        let custom = agents
            .iter()
            .find(|agent| agent.name == "custom")
            .expect("custom exists");
        assert_eq!(
            custom.options.get("reasoningEffort"),
            Some(&Value::String("high".to_owned()))
        );
        assert!(custom.options.contains_key("thinking"));
    }

    #[test]
    fn options_deep_merge_rather_than_replacing() {
        let base = map(serde_json::json!({
            "custom": { "options": { "keep": 1, "nested": { "a": 1 } } },
        }));
        let overlay = map(serde_json::json!({
            "custom": { "options": { "nested": { "b": 2 } } },
        }));
        let merged = merge_agent_maps(&base, &overlay).expect("merge succeeds");
        let agents = resolve(&merged, &[]);
        let custom = agents
            .iter()
            .find(|agent| agent.name == "custom")
            .expect("custom exists");
        assert_eq!(custom.options.get("keep"), Some(&Value::from(1)));
        assert_eq!(
            custom.options.get("nested"),
            Some(&serde_json::json!({ "a": 1, "b": 2 }))
        );
    }

    #[test]
    fn merging_agent_layers_keeps_the_base_permission_rule_order() {
        // `$HOME/.ssh/*` sorts before `*`, so a merge through `serde_json::Value`
        // moves the deny above the catch-all allow and, with last-match-wins
        // precedence, hands the agent read access to the key it denied.
        let base = map_text(
            r#"{"custom":{"permission":{"rules":{"read":{"*":"allow","$HOME/.ssh/*":"deny"}}}}}"#,
        );
        let overlay = map_text(r#"{"custom":{"permission":{"rules":{"read":{"tmp/*":"allow"}}}}}"#);

        let merged = merge_agent_maps(&base, &overlay).expect("merge succeeds");
        let rule = merged
            .get("custom")
            .and_then(|config| config.permission.as_ref())
            .expect("the merged agent keeps its policy")
            .rules
            .get("read")
            .expect("the read rule survives the merge")
            .clone();

        let PermissionRule::Patterns(patterns) = rule else {
            panic!("expected per-pattern rules, got {rule:?}");
        };
        assert_eq!(
            patterns.keys().collect::<Vec<_>>(),
            ["*", "$HOME/.ssh/*", "tmp/*"],
            "the base layer sets the order, and an overlay-only pattern is appended"
        );
    }

    #[test]
    fn a_key_both_layers_set_keeps_the_base_position_and_takes_the_overlay_value() {
        // The rule config discovery applies between two `zuno.json` layers, pinned
        // on the agent path so the two cannot drift apart: the base wrote its
        // catch-all last, and an overlay restating `edit` does not move it.
        let base = map_text(r#"{"build":{"permission":{"rules":{"edit":"deny","*":"deny"}}}}"#);
        let overlay = map_text(r#"{"build":{"permission":{"rules":{"edit":"allow"}}}}"#);
        let merged = merge_agent_maps(&base, &overlay).expect("merge succeeds");
        assert_eq!(rule_keys(&merged, "build"), ["edit", "*"]);
        assert_eq!(
            merged
                .get("build")
                .and_then(|config| config.permission.as_ref())
                .and_then(|permission| permission.rules.get("edit")),
            Some(&PermissionRule::Action(PermissionAction::Allow)),
            "the overlay's value replaces the base's value in place"
        );

        // A key only the overlay names is appended, so it becomes the last match.
        let base = map_text(r#"{"build":{"permission":{"rules":{"shell":"deny"}}}}"#);
        let overlay = map_text(r#"{"build":{"permission":{"rules":{"*":"allow"}}}}"#);
        let merged = merge_agent_maps(&base, &overlay).expect("merge succeeds");
        assert_eq!(rule_keys(&merged, "build"), ["shell", "*"]);
    }

    #[test]
    fn required_skills_survive_agent_merge_and_the_overlay_replaces_the_list() {
        let base = map(serde_json::json!({
            "custom": { "requiredSkills": ["codegraph", "review"] },
        }));
        let overlay = map(serde_json::json!({
            "custom": { "requiredSkills": ["security-review"] },
        }));
        let merged = merge_agent_maps(&base, &overlay).expect("merge succeeds");
        assert_eq!(
            merged
                .get("custom")
                .and_then(|config| config.required_skills.as_deref()),
            Some(["security-review".to_owned()].as_slice())
        );

        let agents = resolve(&merged, &[]);
        let custom = agents
            .iter()
            .find(|agent| agent.name == "custom")
            .expect("custom exists");
        assert_eq!(
            custom.required_skills.as_deref(),
            Some(["security-review".to_owned()].as_slice())
        );
    }

    #[test]
    fn resolved_agent_serializes_required_skills_with_the_public_field_name() {
        let agents = resolve(
            &map(serde_json::json!({
                "custom": { "requiredSkills": ["codegraph", "review"] },
            })),
            &[],
        );
        let custom = agents
            .iter()
            .find(|agent| agent.name == "custom")
            .expect("custom exists");
        let serialized = serde_json::to_value(custom).expect("agent serializes");

        assert_eq!(
            serialized.get("requiredSkills"),
            Some(&serde_json::json!(["codegraph", "review"]))
        );
        assert!(
            serialized.get("required_skills").is_none(),
            "the Rust field name must not leak into the public JSON shape"
        );
    }

    #[test]
    fn a_name_key_renames_the_agent() {
        let agents = resolve(
            &map(serde_json::json!({ "original": { "name": "renamed" } })),
            &[],
        );
        assert!(names(&agents).contains(&"renamed"));
        assert!(!names(&agents).contains(&"original"));
    }

    #[test]
    fn a_name_key_is_not_swept_into_provider_options() {
        let agents = resolve(
            &map(serde_json::json!({ "original": { "name": "renamed" } })),
            &[],
        );
        let renamed = agents
            .iter()
            .find(|agent| agent.name == "renamed")
            .expect("renamed exists");
        assert!(
            !renamed.options.contains_key("name"),
            "SWEEP_EXEMPT_KEYS must keep `name` out of options"
        );
    }

    #[test]
    fn list_puts_natives_first_then_sorts_by_name() {
        let agents = list(
            &map(serde_json::json!({
                "zebra": {},
                "alpha": {},
                "plan": { "mode": "all" },
            })),
            &[],
        );
        let mut expected = builtin::all()
            .into_iter()
            .map(|agent| agent.name)
            .collect::<Vec<_>>();
        expected.sort_by(|left, right| locale_compare(left, right));
        expected.extend(["alpha", "zebra"]);
        assert_eq!(names(&agents), expected);
    }

    #[test]
    fn the_header_is_the_line_agent_list_prints() {
        let agents = list(&OrderedMap::new(), &[]);
        let headers: Vec<String> = agents.iter().map(Agent::header).collect();
        assert_eq!(headers[0], "build (primary)");
        assert!(headers.contains(&"orchestrator (primary)".to_owned()));
        assert!(headers.contains(&"build (primary)".to_owned()));
        assert!(headers.contains(&"deep (all)".to_owned()));
    }

    #[test]
    fn markdown_provenance_reaches_the_resolved_agent() {
        let paths = vec![(
            "review/security".to_owned(),
            PathBuf::from("/cfg/agent/review/security.md"),
        )];
        let agents = resolve(&map(serde_json::json!({ "review/security": {} })), &paths);
        let agent = agents
            .iter()
            .find(|agent| agent.name == "review/security")
            .expect("the markdown agent exists");
        assert_eq!(
            agent.source,
            AgentSource::Markdown {
                path: PathBuf::from("/cfg/agent/review/security.md"),
            }
        );
    }

    #[test]
    fn locale_compare_orders_lowercase_before_a_later_uppercase() {
        // A byte comparison would put uppercase `B` before lowercase `a`.
        assert_eq!(locale_compare("a", "B"), std::cmp::Ordering::Less);
        assert_eq!(locale_compare("B", "a"), std::cmp::Ordering::Greater);
        assert_eq!(locale_compare("a", "a"), std::cmp::Ordering::Equal);
    }
}
